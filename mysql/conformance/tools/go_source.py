"""Read-only lexical access to Go source files used as SQL test fixtures.

The Go importers never build or run upstream Go code. They tokenize the source,
parse composite literals (``[]T{...}``, ``T{Field: value}``) and decode string
literals exactly as the Go compiler would: a Go string is a byte sequence, so
``\\xNN`` and octal escapes contribute single bytes and the result is decoded as
UTF-8 afterwards.
"""

import dataclasses

_SIMPLE_ESCAPES = {
    "a": 0x07,
    "b": 0x08,
    "f": 0x0C,
    "n": 0x0A,
    "r": 0x0D,
    "t": 0x09,
    "v": 0x0B,
    "\\": 0x5C,
    '"': 0x22,
}


def decode_string_bytes(literal):
    """Decode one Go string literal (including its quotes) to raw bytes."""
    if literal.startswith("`"):
        if len(literal) < 2 or not literal.endswith("`"):
            raise ValueError("unterminated raw string")
        # Go discards carriage returns inside raw string literals.
        return literal[1:-1].replace("\r", "").encode("utf-8")
    if len(literal) < 2 or not (literal.startswith('"') and literal.endswith('"')):
        raise ValueError(f"not a Go string literal: {literal[:20]!r}")
    body = literal[1:-1]
    out = bytearray()
    index = 0
    length = len(body)
    while index < length:
        character = body[index]
        if character != "\\":
            out += character.encode("utf-8")
            index += 1
            continue
        if index + 1 >= length:
            raise ValueError("dangling backslash in Go string")
        escape = body[index + 1]
        if escape in _SIMPLE_ESCAPES:
            out.append(_SIMPLE_ESCAPES[escape])
            index += 2
        elif escape == "x":
            out.append(int(body[index + 2 : index + 4], 16))
            index += 4
        elif escape in "01234567":
            value = int(body[index + 1 : index + 4], 8)
            if value > 255:
                raise ValueError("octal escape out of range")
            out.append(value)
            index += 4
        elif escape == "u":
            out += chr(int(body[index + 2 : index + 6], 16)).encode("utf-8")
            index += 6
        elif escape == "U":
            out += chr(int(body[index + 2 : index + 10], 16)).encode("utf-8")
            index += 10
        else:
            raise ValueError(f"unknown Go escape \\{escape}")
    return bytes(out)


def decode_string(literal):
    """Decode a Go string literal to text. Raises UnicodeDecodeError for non-UTF-8 bytes."""
    return decode_string_bytes(literal).decode("utf-8")


@dataclasses.dataclass
class Token:
    kind: str  # string, rune, ident, number, op
    text: str  # exact source text
    line: int
    start: int
    end: int


_OPERATORS = sorted(
    """<<= >>= &^= ... && || <- ++ -- == != <= >= := += -= *= /= %= &= |= ^= << >> &^
    + - * / % & | ^ < > = ! ( ) [ ] { } , ; . : ~""".split(),
    key=len,
    reverse=True,
)


def _needs_semicolon(token):
    if token is None:
        return False
    if token.kind in ("ident", "number", "string", "rune"):
        return True
    return token.kind == "op" and token.text in (")", "]", "}", "++", "--")


def tokenize(source):
    """Tokenize Go source, inserting semicolons at line ends as the Go spec does."""
    tokens = []
    index = 0
    line = 1
    length = len(source)

    def newline():
        if tokens and _needs_semicolon(tokens[-1]):
            tokens.append(Token("op", ";", line, index, index))

    while index < length:
        character = source[index]
        if character == "\n":
            newline()
            line += 1
            index += 1
            continue
        if character.isspace():
            index += 1
            continue
        if source.startswith("//", index):
            end = source.find("\n", index)
            index = length if end < 0 else end
            continue
        if source.startswith("/*", index):
            end = source.find("*/", index + 2)
            if end < 0:
                raise ValueError(f"unterminated comment at line {line}")
            if "\n" in source[index:end]:
                newline()
            line += source.count("\n", index, end)
            index = end + 2
            continue
        start = index
        start_line = line
        if character == "`":
            end = source.find("`", index + 1)
            if end < 0:
                raise ValueError(f"unterminated raw string at line {line}")
            index = end + 1
            line += source.count("\n", start, index)
            tokens.append(Token("string", source[start:index], start_line, start, index))
            continue
        if character in "\"'":
            index += 1
            while index < length and source[index] != character:
                if source[index] == "\n":
                    raise ValueError(f"newline in Go literal at line {start_line}")
                index += 2 if source[index] == "\\" else 1
            if index >= length:
                raise ValueError(f"unterminated Go literal at line {start_line}")
            index += 1
            kind = "string" if character == '"' else "rune"
            tokens.append(Token(kind, source[start:index], start_line, start, index))
            continue
        if character.isalpha() or character == "_" or ord(character) > 127:
            while index < length and (source[index].isalnum() or source[index] == "_" or ord(source[index]) > 127):
                index += 1
            tokens.append(Token("ident", source[start:index], start_line, start, index))
            continue
        if character.isdigit() or (character == "." and index + 1 < length and source[index + 1].isdigit()):
            while index < length and (source[index].isalnum() or source[index] in "._"):
                if source[index] in "eEpP" and index + 1 < length and source[index + 1] in "+-":
                    index += 1
                index += 1
            tokens.append(Token("number", source[start:index], start_line, start, index))
            continue
        for operator in _OPERATORS:
            if source.startswith(operator, index):
                index += len(operator)
                tokens.append(Token("op", operator, start_line, start, index))
                break
        else:
            raise ValueError(f"unexpected character {character!r} at line {line}")
    newline()
    return tokens


_OPEN = {"(": ")", "[": "]", "{": "}"}


def matching(tokens):
    """Map the index of every opening bracket to its closing bracket."""
    stack = []
    pairs = {}
    for index, token in enumerate(tokens):
        if token.kind != "op":
            continue
        if token.text in _OPEN:
            stack.append(index)
        elif token.text in (")", "]", "}"):
            if not stack or _OPEN[tokens[stack[-1]].text] != token.text:
                raise ValueError(f"unbalanced {token.text!r} at line {token.line}")
            pairs[stack.pop()] = index
    if stack:
        raise ValueError(f"unclosed {tokens[stack[-1]].text!r} at line {tokens[stack[-1]].line}")
    return pairs


@dataclasses.dataclass
class Node:
    """A parsed Go value inside a composite literal.

    ``kind`` is ``string`` (value is the decoded text, or ``None`` when the bytes
    are not valid UTF-8), ``composite`` (value is a list of ``(key, Node)`` pairs,
    key ``None`` for positional elements), ``ident`` (a bare or qualified name) or
    ``expr`` (anything else; value is the source text).
    """

    kind: str
    value: object
    line: int
    type: str = ""
    text: str = ""
    token_index: int = 0

    def field(self, name):
        if self.kind != "composite":
            return None
        for key, node in self.value:
            if key == name:
                return node
        return None

    def elements(self):
        if self.kind != "composite":
            return []
        return [node for _key, node in self.value]


class Parser:
    def __init__(self, source):
        self.source = source
        self.tokens = tokenize(source)
        self.pairs = matching(self.tokens)

    def text(self, start, end):
        """Source text of tokens[start:end]."""
        if start >= end:
            return ""
        return self.source[self.tokens[start].start : self.tokens[end - 1].end]

    def is_op(self, index, text):
        return index < len(self.tokens) and self.tokens[index].kind == "op" and self.tokens[index].text == text

    def skip_to_stop(self, index, stops):
        """Skip a balanced expression up to a stop token at the same depth."""
        while index < len(self.tokens):
            token = self.tokens[index]
            if token.kind == "op":
                if token.text in stops:
                    return index
                if token.text in _OPEN:
                    index = self.pairs[index] + 1
                    continue
            index += 1
        return index

    def parse_value(self, index, stops=(",", "}", ";")):
        """Parse the value starting at ``index``; return ``(node, next_index)``."""
        start = index
        token = self.tokens[index]
        node = None
        if token.kind == "string":
            pieces = [token]
            index += 1
            while self.is_op(index, "+") and index + 1 < len(self.tokens) and self.tokens[index + 1].kind == "string":
                pieces.append(self.tokens[index + 1])
                index += 2
            raw = b"".join(decode_string_bytes(piece.text) for piece in pieces)
            try:
                value = raw.decode("utf-8")
            except UnicodeDecodeError:
                value = None
            node = Node("string", value, token.line)
        else:
            # Optional & before a composite literal.
            type_start = index + 1 if self.is_op(index, "&") else index
            brace = self._composite_brace(type_start)
            if brace is not None:
                node = self.parse_composite(brace)
                node.type = self.text(type_start, brace)
                index = self.pairs[brace] + 1
            elif token.kind == "ident":
                index += 1
                while self.is_op(index, ".") and index + 1 < len(self.tokens) and self.tokens[index + 1].kind == "ident":
                    index += 2
                node = Node("ident", self.text(start, index), token.line)
        end = self.skip_to_stop(index, stops)
        if node is None or end != index:
            node = Node("expr", self.text(start, end), token.line)
        node.text = self.text(start, end)
        node.token_index = start
        return node, end

    def _composite_brace(self, index):
        """Index of ``{`` if a composite literal starts at ``index``."""
        tokens = self.tokens
        if self.is_op(index, "{"):
            return index
        cursor = index
        # Type prefixes: []T, [N]T, map[K]V, *T, pkg.T
        while cursor < len(tokens):
            token = tokens[cursor]
            if token.kind == "op" and token.text == "[":
                cursor = self.pairs[cursor] + 1
                continue
            if token.kind == "op" and token.text == "*":
                cursor += 1
                continue
            if token.kind == "ident":
                if token.text == "map" and self.is_op(cursor + 1, "["):
                    cursor = self.pairs[cursor + 1] + 1
                    continue
                if token.text == "struct" and self.is_op(cursor + 1, "{"):
                    cursor = self.pairs[cursor + 1] + 1
                    break
                if token.text in ("func", "chan", "interface"):
                    return None
                cursor += 1
                while self.is_op(cursor, ".") and cursor + 1 < len(tokens) and tokens[cursor + 1].kind == "ident":
                    cursor += 2
                break
            return None
        if cursor > index and self.is_op(cursor, "{"):
            return cursor
        return None

    def parse_composite(self, brace):
        close = self.pairs[brace]
        elements = []
        index = brace + 1
        while index < close:
            if self.is_op(index, ","):
                index += 1
                continue
            key = None
            token = self.tokens[index]
            if token.kind == "ident" and self.is_op(index + 1, ":"):
                key = token.text
                index += 2
            node, index = self.parse_value(index)
            elements.append((key, node))
        return Node("composite", elements, self.tokens[brace].line)

    def declarations(self, start=0, end=None):
        """Yield ``(name, value_node)`` for ``var name [T] = v`` and ``name := v``.

        Only simple single-name declarations are reported; grouped ``var ( ... )``
        blocks are handled. ``start``/``end`` restrict the token range.
        """
        tokens = self.tokens
        end = len(tokens) if end is None else end
        index = start
        while index < end:
            token = tokens[index]
            if token.kind == "ident" and self.is_op(index + 1, ":="):
                node, _ = self.parse_value(index + 2, (";",))
                yield token.text, node
                index += 2
                continue
            if token.kind == "ident" and token.text == "var":
                if self.is_op(index + 1, "("):
                    close = self.pairs[index + 1]
                    cursor = index + 2
                    while cursor < close:
                        cursor = yield from self._var_spec(cursor, close)
                    index = close + 1
                    continue
                index = yield from self._var_spec(index + 1, end)
                continue
            index += 1

    def statements(self, start, end):
        """Yield ``(name, op, value_node)`` for ``name := v`` and ``name = v`` in a range."""
        tokens = self.tokens
        index = start
        while index < end:
            token = tokens[index]
            if (
                token.kind == "ident"
                and (index == start or self.is_op(index - 1, ";") or self.is_op(index - 1, "{"))
                and (self.is_op(index + 1, ":=") or self.is_op(index + 1, "="))
            ):
                node, after = self.parse_value(index + 2, (";",))
                yield token.text, tokens[index + 1].text, node
                index = after
                continue
            index += 1

    def call_arguments(self, node):
        """Parse the arguments of a call expression node such as ``append(a, b...)``."""
        open_index = node.token_index
        while not self.is_op(open_index, "("):
            open_index += 1
        close = self.pairs[open_index]
        arguments = []
        index = open_index + 1
        while index < close:
            if self.is_op(index, ","):
                index += 1
                continue
            value, index = self.parse_value(index, (",", ")"))
            arguments.append(value)
        return arguments

    def _var_spec(self, index, end):
        tokens = self.tokens
        if index >= end or tokens[index].kind != "ident":
            return index + 1
        name = tokens[index].text
        cursor = index + 1
        while cursor < end and not self.is_op(cursor, "=") and not self.is_op(cursor, ";"):
            if tokens[cursor].kind == "op" and tokens[cursor].text in _OPEN:
                cursor = self.pairs[cursor] + 1
                continue
            cursor += 1
        if self.is_op(cursor, "="):
            node, cursor = self.parse_value(cursor + 1, (";", ")"))
            yield name, node
        return self.skip_to_stop(cursor, (";",)) + 1


def function_spans(parser):
    """Map top-level function names to ``(open_brace_index, close_brace_index)``."""
    tokens = parser.tokens
    spans = {}
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if token.kind == "op" and token.text in _OPEN:
            index = parser.pairs[index] + 1
            continue
        if token.kind == "ident" and token.text == "func":
            cursor = index + 1
            if parser.is_op(cursor, "("):  # method receiver
                cursor = parser.pairs[cursor] + 1
            name = tokens[cursor].text if cursor < len(tokens) and tokens[cursor].kind == "ident" else None
            while cursor < len(tokens) and not parser.is_op(cursor, "{"):
                if parser.is_op(cursor, "(") or parser.is_op(cursor, "["):
                    cursor = parser.pairs[cursor] + 1
                    continue
                cursor += 1
            if cursor < len(tokens) and name:
                spans[name] = (cursor, parser.pairs[cursor], tokens[index].line)
                index = parser.pairs[cursor] + 1
                continue
        index += 1
    return spans
