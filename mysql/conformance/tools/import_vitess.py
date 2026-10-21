#!/usr/bin/env python3
"""Import Vitess evalengine expression cases as MySQL spec candidates.

Vitess (Apache-2.0) validates its expression evaluator against real MySQL with
a combinatorial generator in ``go/vt/vtgate/evalengine/testcases``. This module
is a Python port of that generator's loops (Copyright 2023 The Vitess Authors,
Apache License 2.0; see ``licenses/mysql/vitess-apache-license.md``). Each
generated expression becomes one setup-free ``SELECT <expr>`` candidate; the
expected value is recorded later from MySQL 8.4.10, never taken from Vitess.

Faithfulness checks, all run on every import:

* the three Go source files must match pinned SHA-256 hashes, so the port is
  never silently applied to a changed generator;
* every string list is read from the Go source itself (``[]string{...}``
  literals, ``append`` chains and a few known ``strconv``/``format`` calls), not
  transcribed by hand;
* every other string literal inside each Go family function must appear among
  the constants of the matching Python function, which catches mistyped format
  strings and inline lists.

The Go sources are parsed as text; nothing from the Vitess tree is executed.
"""

import argparse
import base64
import collections
import decimal
import hashlib
import heapq
import inspect
import math
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import go_source  # noqa: E402
import importer_common as common  # noqa: E402

SOURCE_NAME = "vitess"
REPO = "https://github.com/vitessio/vitess.git"
REVISION = "e157537374a15996987346ea6d356d1ca7e8aeec"
LICENSE = "Apache-2.0"
TESTCASES = "go/vt/vtgate/evalengine/testcases"
FILE_HASHES = {
    "cases.go": "0d1f149b0b2b970bdc6da26db4d14bd615ad77efab9bca82c6f173ec79bfdb4f",
    "helpers.go": "b965fce2af6adc19a1a7844db3ea55c7a9b81ac621642ef73a6aec731bfe3489",
    "inputs.go": "fc697e5ca0a70b4dc0adc7ae50d7367b250b8165325f12b8bc3fe94ee89b0803",
}
DEFAULT_TARGET = 150_000

# Families skipped entirely, with the reason recorded in the import report.
EXCLUDED_FAMILIES = {
    "FnNow": "nondeterministic: current time (NOW/SYSDATE/UTC_*/CURDATE/CURTIME family)",
    "FnInfo": "server state: USER()/CURRENT_USER/DATABASE()/VERSION() depend on the connection",
}
# Individual expressions skipped, with reasons.
EXCLUDED_EXPRESSIONS = {
    "UNIX_TIMESTAMP()": "nondeterministic: current time",
}

# Non-[]string declarations whose contents are read from the Go source.
READ_FROM_SOURCE = {("JSONExtract", "cases"), ("JSONExtract", "rows")}

# Table used for families with a row schema, as in the Vitess MySQL comparison harness.
VTEVAL_TABLE = "vteval_test"


# ---------------------------------------------------------------------------
# Go formatting helpers
# ---------------------------------------------------------------------------


def go_quote(text):
    """strconv.Quote for the strings the generator quotes with %q."""
    out = ['"']
    for character in text:
        if character in ('"', "\\"):
            out.append("\\" + character)
        elif character.isprintable():
            out.append(character)
        elif character in "\a\b\f\n\r\t\v":
            out.append({"\a": "\\a", "\b": "\\b", "\f": "\\f", "\n": "\\n", "\r": "\\r", "\t": "\\t", "\v": "\\v"}[character])
        elif ord(character) < 0x80:
            out.append(f"\\x{ord(character):02x}")
        elif ord(character) <= 0xFFFF:
            out.append(f"\\u{ord(character):04x}")
        else:
            out.append(f"\\U{ord(character):08x}")
    out.append('"')
    return "".join(out)


_VERB = re.compile(r"%[sdqv%]")


def S(template, *arguments):
    """fmt.Sprintf for the verbs used by the generator (%s, %d, %v, %q)."""
    values = iter(arguments)

    def replace(match):
        verb = match.group()
        if verb == "%%":
            return "%"
        value = next(values)
        if verb == "%q":
            return go_quote(value)
        return str(value)

    result = _VERB.sub(replace, template)
    leftover = list(values)
    if leftover:
        raise ValueError(f"unused Sprintf arguments for {template!r}: {leftover!r}")
    return result


def go_format_float(value, verb):
    """strconv.FormatFloat(value, verb, -1, 64) for verb 'f' or 'g' (finite values)."""
    if value == 0:
        return "-0" if math.copysign(1.0, value) < 0 else "0"
    sign = "-" if value < 0 else ""
    digits_tuple = decimal.Decimal(repr(abs(value))).normalize().as_tuple()
    digits = "".join(str(d) for d in digits_tuple.digits)
    point = len(digits) + digits_tuple.exponent  # decimal point position
    if verb == "g":
        exponent = point - 1
        if exponent < -4 or exponent >= 6:  # shortest 'g' uses %e with eprec 6
            mantissa = digits[0] + ("." + digits[1:] if len(digits) > 1 else "")
            return f"{sign}{mantissa}e{'-' if exponent < 0 else '+'}{abs(exponent):02d}"
    if point <= 0:
        return f"{sign}0.{'0' * -point}{digits}"
    if point >= len(digits):
        return f"{sign}{digits}{'0' * (point - len(digits))}"
    return f"{sign}{digits[:point]}.{digits[point:]}"


def vitess_format_float(value):
    """Port of vitess go/mysql/format.FormatFloat."""
    verb = "f"
    if value >= 1e15 or value <= -1e15 or (-1e-15 < value < 1e-15):
        verb = "g"
    text = go_format_float(value, verb)
    index = text.find("e")
    if index >= 0 and text[index + 1] == "+":
        text = text[: index + 1] + text[index + 2 :]
    return text


GO_CONSTANTS = {
    "math.MaxUint64": 2**64 - 1,
    "math.MaxInt64": 2**63 - 1,
    "math.MinInt64": -(2**63),
    "math.MaxInt32": 2**31 - 1,
    "math.MinInt32": -(2**31),
    "math.MaxUint32": 2**32 - 1,
}
GO_FLOATS = {
    "math.Pi": math.pi,
    "math.MaxFloat64": 1.7976931348623157e308,
    "math.SmallestNonzeroFloat32": 2.0**-149,
    "math.SmallestNonzeroFloat64": 5e-324,
}
_STRCONV = re.compile(r"strconv\.Format(?:Uint|Int)\((math\.\w+)\s*(?:([+-])\s*(\d+))?,\s*10\)$")
_FORMAT_FLOAT = re.compile(r"string\(format\.FormatFloat\((math\.\w+)\)\)$")


def evaluate_go_string_expression(text):
    match = _STRCONV.match(text)
    if match:
        value = GO_CONSTANTS[match.group(1)]
        if match.group(2):
            value = value + int(match.group(3)) if match.group(2) == "+" else value - int(match.group(3))
        return str(value)
    match = _FORMAT_FLOAT.match(text)
    if match:
        return vitess_format_float(GO_FLOATS[match.group(1)])
    raise ValueError(f"unsupported Go string expression: {text}")


# ---------------------------------------------------------------------------
# Lists read from the Go source
# ---------------------------------------------------------------------------


class GoInputs:
    """String lists and literals parsed from the Vitess testcases package."""

    def __init__(self, directory):
        directory = pathlib.Path(directory)
        self.directory = directory
        self.sources = {name: (directory / name).read_text(encoding="utf-8") for name in FILE_HASHES}
        self.cases = go_source.Parser(self.sources["cases.go"])
        self.inputs = go_source.Parser(self.sources["inputs.go"])
        self.globals = {}
        self.global_nodes = {}
        for name, node in self.inputs.declarations():
            self.global_nodes[name] = node
            if node.kind == "composite" and node.type == "[]string":
                self.globals[name] = [self.string_value(element) for element in node.elements()]
        for name, node in self.cases.declarations(0, None):
            if name == "Cases":
                self.case_names = [element.field("Run").text for element in node.elements()]
        self.functions = go_source.function_spans(self.cases)
        self.locals = {}
        self.local_literal_spans = collections.defaultdict(list)
        for function, (open_index, close_index, _line) in self.functions.items():
            environment = {}
            for name, _op, node in self.cases.statements(open_index + 1, close_index):
                value = self.evaluate_list(node, environment)
                if value is not None:
                    environment[name] = value
                    self.locals[(function, name)] = value
                if value is not None or (function, name) in READ_FROM_SOURCE:
                    span = (node.token_index, self.cases.skip_to_stop(node.token_index, (";",)))
                    self.local_literal_spans[function].append(span)

    def string_value(self, node):
        if node.kind == "string":
            if node.value is None:
                raise ValueError(f"non-UTF-8 Go string at line {node.line}")
            return node.value
        return evaluate_go_string_expression(node.text)

    def evaluate_list(self, node, environment):
        """Evaluate a []string-valued expression, or return None if it is not one."""
        if node.kind == "composite" and node.type == "[]string":
            return [self.string_value(element) for element in node.elements()]
        if node.kind == "ident":
            if node.text in environment:
                return list(environment[node.text])
            if node.text in self.globals:
                return list(self.globals[node.text])
            return None
        if node.kind == "expr" and node.text.startswith("append("):
            arguments = self.cases.call_arguments(node)
            base = self.evaluate_list(arguments[0], environment)
            if base is None:
                return None
            for argument in arguments[1:]:
                if argument.kind == "expr" and not argument.text.endswith("..."):
                    # Computed elements (for example base64 of a loop variable) are
                    # produced by the port itself.
                    return None
                if argument.text.endswith("..."):
                    spread = argument.text[:-3]
                    if spread in environment:
                        base += environment[spread]
                    elif spread in self.globals:
                        base += self.globals[spread]
                    else:
                        raise ValueError(f"cannot spread {spread} at line {node.line}")
                else:
                    base.append(self.string_value(argument))
            return base
        if node.kind == "expr" and (node.text.startswith("make([]string") or node.text == "[]string(nil)"):
            return []
        return None

    def g(self, name):
        return self.globals[name]

    def l(self, function, name):
        return self.locals[(function, name)]

    def date_format_chars(self):
        node = self.global_nodes["dateFormats"]
        chars = []
        for element in node.elements():
            rune = element.elements()[0].text
            if not (len(rune) == 3 and rune[0] == rune[2] == "'"):
                raise ValueError(f"unexpected dateFormats rune {rune}")
            chars.append(rune[1])
        return chars

    def json_extract_cases(self):
        node = self._local_node("JSONExtract", "cases")
        return [(element.field("Operator").value, element.field("Path").value) for element in node.elements()]

    def json_extract_rows(self):
        node = self._local_node("JSONExtract", "rows")
        rows = []
        for element in node.elements():
            if not element.text.startswith("mustJSON("):
                raise ValueError(f"unexpected JSONExtract row {element.text}")
            (argument,) = self.cases.call_arguments(element)
            rows.append(self.string_value(argument))
        return rows

    def _local_node(self, function, name):
        open_index, close_index, _line = self.functions[function]
        for found, _op, node in self.cases.statements(open_index + 1, close_index):
            if found == name:
                return node
        raise KeyError((function, name))

    def function_literals(self, function):
        """Decoded string literals in a Go function body outside list declarations."""
        open_index, close_index, _line = self.functions[function]
        skipped = self.local_literal_spans.get(function, [])
        literals = []
        for index in range(open_index, close_index):
            token = self.cases.tokens[index]
            if token.kind != "string":
                continue
            if any(start <= index < end for start, end in skipped):
                continue
            literals.append(go_source.decode_string(token.text))
        return literals


# ---------------------------------------------------------------------------
# Port of bugs{} from helpers.go
# ---------------------------------------------------------------------------


def can_compare(*elems):
    invalid = {
        "18446744073709551615": "-1",
        "9223372036854775808": "-9223372036854775808",
    }
    for i, e in enumerate(elems):
        if e.startswith("_binary ") or e.startswith("0x") or e.startswith("X'") or e.endswith("collate utf8mb4_0900_as_cs"):
            return False
        if e in invalid:
            other = invalid[e]
            for j in range(len(elems)):
                if i != j and elems[j] == other:
                    return False
    return True


def make_time_valid_hours(text):
    if not re.fullmatch(r"[+-]?[0-9]+", text):
        return True
    value = int(text)
    if value < -(2**63) or value > 2**63 - 1:
        return True
    return value != -(2**63)


def perm(a, f):
    a = list(a)

    def perm1(i):
        if i > len(a):
            f(list(a))
            return
        perm1(i + 1)
        for j in range(i + 1, len(a)):
            a[i], a[j] = a[j], a[i]
            perm1(i + 1)
            a[i], a[j] = a[j], a[i]

    perm1(0)


def gen_subsets(args, subset_len):
    """Yield subsets in the order of helpers.go genSubsets."""
    subset = [None] * subset_len
    out = []

    def gen(a, b):
        if a == len(subset):
            out.append(list(subset))
            return
        if b >= len(args):
            return
        subset[a] = args[b]
        gen(a + 1, b + 1)
        gen(a, b + 1)

    gen(0, 0)
    return out


# ---------------------------------------------------------------------------
# Port of cases.go. Each function yields an expression, or (expression, row)
# for families with a row schema. Go format strings are kept verbatim.
# ---------------------------------------------------------------------------


def JSONExtract(G):
    for operator, path in G.json_extract_cases():
        expr0 = S("column0%s'%s'", operator, path)
        expr1 = S("cast(json_unquote(json_extract(column0, '%s')) as char)", path)
        expr2 = S("cast(%s as char) <=> %s", expr0, expr1)
        for row in G.json_extract_rows():
            yield expr0, row
            yield expr1, row
            yield expr2, row


def FnJSONKeys(G):
    for obj in G.g("inputJSONObjects"):
        yield S("JSON_KEYS(%s)", obj)
        for path1 in G.g("inputJSONPaths"):
            yield S("JSON_KEYS(%s, '%s')", obj, path1)


def FnJSONExtract(G):
    for obj in G.g("inputJSONObjects"):
        for path1 in G.g("inputJSONPaths"):
            yield S("JSON_EXTRACT(%s, '%s')", obj, path1)
            for path2 in G.g("inputJSONPaths"):
                yield S("JSON_EXTRACT(%s, '%s', '%s')", obj, path1, path2)
    yield """JSON_EXTRACT('{"a": 1}', '$.a')"""
    yield """JSON_EXTRACT('{"a": 1}', '$.*')"""
    yield """JSON_EXTRACT('[1, 2, 3]', '$[0 to 2]')"""
    yield """JSON_EXTRACT('{"a": 1, "b": 2}', '$.a', '$.b')"""
    yield """JSON_EXTRACT('{"a": 1}', '$.a', '$.z')"""
    yield """JSON_EXTRACT(CONCAT('{', '"a"', ':', ' ', '1', '}'), '$.a')"""
    yield """JSON_EXTRACT('{"a": 1}', CONCAT('$', '.', 'a'))"""
    yield """JSON_EXTRACT(NULL, '$.a')"""
    yield """JSON_EXTRACT(NULL, NULL)"""
    yield """JSON_EXTRACT('{"a": 1}', NULL)"""
    yield """JSON_EXTRACT('{"a": 1}', '$.a', NULL)"""
    yield """JSON_EXTRACT('{"a": 1}', NULL, '$.a')"""
    yield """JSON_EXTRACT('{"a": 1}', '$.b')"""
    yield """JSON_EXTRACT('{"a": 1}', '$.b', '$.c')"""
    yield """JSON_EXTRACT('[1,2,3]', '$[10]')"""
    yield """JSON_EXTRACT('{invalid}', '$.a')"""
    yield """JSON_EXTRACT('not json', '$.a')"""
    yield """JSON_EXTRACT('', '$.a')"""
    yield """JSON_EXTRACT('{invalid}', NULL)"""
    yield """JSON_EXTRACT('{"a": 1}', '$.b[ 1 ].')"""
    yield """JSON_EXTRACT(NULL, 'invalid-path')"""
    yield """JSON_EXTRACT('{"a": 1}', NULL, 'invalid-path')"""
    yield """JSON_EXTRACT('{"a": 1}', 'invalid-path', NULL)"""
    yield """JSON_EXTRACT('{"a": 1}', '$.a', 'invalid')"""


def FnJSONRemove(G):
    for obj in G.g("inputJSONObjects"):
        for path1 in G.g("inputJSONPaths"):
            yield S("JSON_REMOVE(%s, '%s')", obj, path1)


def FnJSONContainsPath(G):
    for obj in G.g("inputJSONObjects"):
        for path1 in G.g("inputJSONPaths"):
            yield S("JSON_CONTAINS_PATH(%s, 'one', '%s')", obj, path1)
            yield S("JSON_CONTAINS_PATH(%s, 'all', '%s')", obj, path1)
            for path2 in G.g("inputJSONPaths"):
                yield S("JSON_CONTAINS_PATH(%s, 'one', '%s', '%s')", obj, path1, path2)
                yield S("JSON_CONTAINS_PATH(%s, 'all', '%s', '%s')", obj, path1, path2)


def FnJSONUnquote(G):
    yield "JSON_UNQUOTE(NULL)"


def JSONArray(G):
    primitives = G.g("inputJSONPrimitives")
    for a in primitives:
        yield S("JSON_ARRAY(%s)", a)
        for b in primitives:
            yield S("JSON_ARRAY(%s, %s)", a, b)
    for b in G.g("inputJSONBinaryValues"):
        yield S("JSON_ARRAY(%s)", b)
        yield S("JSON_ARRAY('a', %s)", b)
    yield "JSON_ARRAY()"


def JSONObject(G):
    primitives = G.g("inputJSONPrimitives")
    for a in primitives:
        for b in primitives:
            yield S("JSON_OBJECT(%s, %s)", a, b)
    for b in G.g("inputJSONBinaryValues"):
        yield S("JSON_OBJECT('k', %s)", b)
    yield "JSON_OBJECT()"


def CharsetConversionOperators(G):
    for pfx in G.l("CharsetConversionOperators", "introducers"):
        for lhs in G.l("CharsetConversionOperators", "contents"):
            for rhs in G.l("CharsetConversionOperators", "charsets"):
                yield S("HEX(CONVERT(%s %s USING %s))", pfx, lhs, rhs)


def CaseExprWithPredicate(G):
    predicates = G.l("CaseExprWithPredicate", "predicates")
    elements = G.l("CaseExprWithPredicate", "elements")
    for pred1 in predicates:
        for val1 in elements:
            for else_val in elements:
                yield S("case when %s then %s else %s end", pred1, val1, else_val)
    for preds in gen_subsets(predicates, 3):
        for values in gen_subsets(elements, 3):
            yield S(
                "case when %s then %s when %s then %s when %s then %s end",
                preds[0], values[0], preds[1], values[1], preds[2], values[2],
            )


def FnCeil(G):
    for num in G.l("FnCeil", "ceilInputs"):
        yield S("CEIL(%s)", num)
        yield S("CEILING(%s)", num)
    for num in G.g("inputBitwise"):
        yield S("CEIL(%s)", num)
        yield S("CEILING(%s)", num)


def FnFloor(G):
    for num in G.l("FnFloor", "floorInputs"):
        yield S("FLOOR(%s)", num)
    for num in G.g("inputBitwise"):
        yield S("FLOOR(%s)", num)


def FnAbs(G):
    for num in G.l("FnAbs", "absInputs"):
        yield S("ABS(%s)", num)
    for num in G.g("inputBitwise"):
        yield S("ABS(%s)", num)


def FnPi(G):
    yield "PI()+0.000000000000000000"


def _unary_radian_bitwise(G, template):
    for num in G.g("radianInputs"):
        yield S(template, num)
    for num in G.g("inputBitwise"):
        yield S(template, num)


def FnAcos(G):
    yield from _unary_radian_bitwise(G, "ACOS(%s)")


def FnAsin(G):
    yield from _unary_radian_bitwise(G, "ASIN(%s)")


def FnAtan(G):
    yield from _unary_radian_bitwise(G, "ATAN(%s)")


def FnAtan2(G):
    for num1 in G.g("radianInputs"):
        for num2 in G.g("radianInputs"):
            yield S("ATAN(%s, %s)", num1, num2)
            yield S("ATAN2(%s, %s)", num1, num2)
    for num1 in G.g("inputBitwise"):
        for num2 in G.g("inputBitwise"):
            yield S("ATAN(%s, %s)", num1, num2)
            yield S("ATAN2(%s, %s)", num1, num2)


def FnCos(G):
    yield from _unary_radian_bitwise(G, "COS(%s)")


def FnCot(G):
    yield from _unary_radian_bitwise(G, "COT(%s)")


def FnSin(G):
    yield from _unary_radian_bitwise(G, "SIN(%s)")


def FnTan(G):
    yield from _unary_radian_bitwise(G, "TAN(%s)")


def FnDegrees(G):
    yield from _unary_radian_bitwise(G, "DEGREES(%s)")


def FnRadians(G):
    yield from _unary_radian_bitwise(G, "RADIANS(%s)")


def FnExp(G):
    yield from _unary_radian_bitwise(G, "EXP(%s)")


def FnLn(G):
    yield from _unary_radian_bitwise(G, "LN(%s)")


def _binary_radian_bitwise(G, templates):
    radians = G.g("radianInputs")
    bitwise = G.g("inputBitwise")
    for num1 in radians:
        for num2 in radians:
            for template in templates:
                yield S(template, num1, num2)
        for num2 in bitwise:
            for template in templates:
                yield S(template, num1, num2)
    for num1 in bitwise:
        for num2 in radians:
            for template in templates:
                yield S(template, num1, num2)
        for num2 in bitwise:
            for template in templates:
                yield S(template, num1, num2)


def FnLog(G):
    yield from _unary_radian_bitwise(G, "LOG(%s)")
    yield from _binary_radian_bitwise(G, ("LOG(%s, %s)",))


def FnLog10(G):
    yield from _unary_radian_bitwise(G, "LOG10(%s)")


def FnMod(G):
    yield from _binary_radian_bitwise(G, ("MOD(%s, %s)",))


def FnLog2(G):
    yield from _unary_radian_bitwise(G, "LOG2(%s)")


def FnPow(G):
    yield from _binary_radian_bitwise(G, ("POW(%s, %s)", "POWER(%s, %s)"))


def FnSign(G):
    yield from _unary_radian_bitwise(G, "SIGN(%s)")


def FnSqrt(G):
    yield from _unary_radian_bitwise(G, "SQRT(%s)")


def FnRound(G):
    yield from _unary_radian_bitwise(G, "ROUND(%s)")
    yield from _binary_radian_bitwise(G, ("ROUND(%s, %s)",))


def FnTruncate(G):
    yield from _binary_radian_bitwise(G, ("TRUNCATE(%s, %s)",))


def FnCrc32(G):
    yield from _unary_radian_bitwise(G, "CRC32(%s)")
    for num in G.g("inputConversions"):
        yield S("CRC32(%s)", num)


def FnConv(G):
    radians = G.g("radianInputs")
    bitwise = G.g("inputBitwise")
    for num1 in radians:
        for num2 in radians:
            for num3 in radians:
                yield S("CONV(%s, %s, %s)", num1, num2, num3)
            for num3 in bitwise:
                yield S("CONV(%s, %s, %s)", num1, num2, num3)
    for num1 in radians:
        for num2 in bitwise:
            for num3 in radians:
                yield S("CONV(%s, %s, %s)", num1, num2, num3)
            for num3 in bitwise:
                yield S("CONV(%s, %s, %s)", num1, num2, num3)
    for num1 in bitwise:
        for num2 in bitwise:
            for num3 in radians:
                yield S("CONV(%s, %s, %s)", num1, num2, num3)
            for num3 in bitwise:
                yield S("CONV(%s, %s, %s)", num1, num2, num3)


def FnBin(G):
    yield from _unary_radian_bitwise(G, "BIN(%s)")


def FnOct(G):
    yield from _unary_radian_bitwise(G, "OCT(%s)")


def FnMD5(G):
    yield from _unary_radian_bitwise(G, "MD5(%s)")
    for num in G.g("inputConversions"):
        yield S("MD5(%s)", num)


def FnSHA1(G):
    for source in (G.g("radianInputs"), G.g("inputBitwise"), G.g("inputConversions")):
        for num in source:
            yield S("SHA1(%s)", num)
            yield S("SHA(%s)", num)


def FnSHA2(G):
    for bits in G.l("FnSHA2", "bitLengths"):
        for source in (G.g("radianInputs"), G.g("inputBitwise"), G.g("inputConversions")):
            for num in source:
                yield S("SHA2(%s, %s)", num, bits)


def FnRandomBytes(G):
    for source in (G.g("radianInputs"), G.g("inputBitwise")):
        for num in source:
            yield S("LENGTH(RANDOM_BYTES(%s))", num)
            yield S("COLLATION(RANDOM_BYTES(%s))", num)


def CaseExprWithValue(G):
    elements = G.l("CaseExprWithValue", "elements")
    for cmpbase in elements:
        for val1 in elements:
            if not can_compare(cmpbase, val1):
                continue
            yield S("case %s when %s then 1 else 0 end", cmpbase, val1)


def If(G):
    elements = G.l("If", "elements")
    for cmpbase in elements:
        for val1 in elements:
            for val2 in elements:
                yield S("if(%s, %s, %s)", cmpbase, val1, val2)


def Base64(G):
    inputs = G.l("Base64", "inputs")
    for input_ in G.g("inputConversions"):
        inputs.append("'" + common_base64(input_) + "'")
    for lhs in inputs:
        yield S("FROM_BASE64(%s)", lhs)
        yield S("TO_BASE64(%s)", lhs)


def common_base64(text):
    return base64.b64encode(text.encode("utf-8")).decode("ascii")


def Conversion(G):
    for lhs in G.g("inputConversions"):
        for rhs in G.g("inputConversionTypes"):
            yield S("CAST(%s AS %s)", lhs, rhs)
            yield S("CONVERT(%s, %s)", lhs, rhs)
            yield S("CAST(CAST(%s AS JSON) AS %s)", lhs, rhs)


def LargeDecimals(G):
    largepi = INPUT_PI + INPUT_PI
    for pos in range(len(largepi)):
        yield S("%s.%s", largepi[:pos], largepi[pos:])
        yield S("-%s.%s", largepi[:pos], largepi[pos:])


def LargeIntegers(G):
    largepi = INPUT_PI + INPUT_PI
    for pos in range(1, len(largepi)):
        yield largepi[:pos]
        yield "-" + largepi[:pos]


def DecimalClamping(G):
    for pos in range(len(INPUT_PI)):
        for m in range(0, min(len(INPUT_PI), 67), 2):
            for d in range(0, min(m, 33) + 1, 2):
                yield S("CAST(%s.%s AS DECIMAL(%d, %d))", INPUT_PI[:pos], INPUT_PI[pos:], m, d)


def BitwiseOperatorsUnary(G):
    for op in ["~", "BIT_COUNT"]:
        for rhs in G.g("inputBitwise"):
            yield S("%s(%s)", op, rhs)


def BitwiseOperators(G):
    for op in ["&", "|", "^", "<<", ">>"]:
        for lhs in G.g("inputBitwise"):
            for rhs in G.g("inputBitwise"):
                yield S("%s %s %s", lhs, op, rhs)
        for lhs in G.g("inputConversions"):
            for rhs in G.g("inputConversions"):
                yield S("%s %s %s", lhs, op, rhs)


def WeightString(G):
    for i in G.l("WeightString", "inputs"):
        yield S("WEIGHT_STRING(%s)", i)


def FloatFormatting(G):
    for f in G.l("FloatFormatting", "floats"):
        yield f + " + 0.0e0"
        yield "-" + f
    mask = 2**64 - 1
    for i in range(64):
        v = 1 << i
        yield S("%d + 0.0e0", v)
        yield S("%d + 0.0e0", v + 1)
        yield S("%d + 0.0e0", mask ^ v)
        yield S("-%de0", v)
        yield S("-%de0", v + 1)
        yield S("-%de0", mask ^ v)


def UnderscoreAndPercentage(G):
    yield from G.l("UnderscoreAndPercentage", "queries")


def Types(G):
    yield from G.l("Types", "queries")


def Arithmetic(G):
    for op in G.l("Arithmetic", "operators"):
        for lhs in G.g("inputConversions"):
            for rhs in G.g("inputConversions"):
                yield S("%s %s %s", lhs, op, rhs)
        for lhs in G.g("inputBitwise"):
            for rhs in G.g("inputBitwise"):
                yield S("%s %s %s", lhs, op, rhs)


def HexArithmetic(G):
    cases = G.l("HexArithmetic", "cases")
    for lhs in cases:
        for rhs in cases:
            yield S("%s + %s", lhs, rhs)
            yield S("-%s + -%s", lhs, rhs)


def NumericTypes(G):
    yield from G.l("NumericTypes", "numbers")


def NegateArithmetic(G):
    for rhs in G.l("NegateArithmetic", "cases"):
        yield "- " + rhs
        yield "-" + rhs
    for rhs in G.g("inputConversions"):
        yield "- " + rhs
        yield "-" + rhs


def CollationOperations(G):
    yield from G.l("CollationOperations", "cases")


def LikeComparison(G):
    for lhs in G.l("LikeComparison", "left"):
        for rhs in G.l("LikeComparison", "right"):
            for op in ["LIKE", "NOT LIKE"]:
                yield S("%s %s %s", lhs, op, rhs)


def StrcmpComparison(G):
    inputs = G.l("StrcmpComparison", "inputs")
    for lhs in inputs:
        for rhs in inputs:
            yield S("STRCMP(%s, %s)", lhs, rhs)


def MultiComparisons(G):
    numbers = G.l("MultiComparisons", "numbers")
    for method in ["LEAST", "GREATEST"]:
        # The Go skip() callback only relaxes Vitess' collation check; it does
        # not change which expressions are generated (it uses "date'",
        # "time'" and "timestamp'" markers).
        for arg in gen_subsets(numbers, 2):
            yield S("%s(%s, %s)", method, arg[0], arg[1])
            yield S("%s(%s, %s)", method, arg[1], arg[0])
        for arg in gen_subsets(numbers, 3):
            yield S("%s(%s, %s, %s)", method, arg[0], arg[1], arg[2])
            yield S("%s(%s, %s, %s)", method, arg[2], arg[1], arg[0])


MULTI_COMPARISON_SKIP_MARKERS = ("date'", "time'", "timestamp'")


def IntervalStatement(G):
    inputs = G.l("IntervalStatement", "inputs")
    for base in inputs:
        for arg1 in inputs:
            for arg2 in inputs:
                for arg3 in inputs:
                    yield S("INTERVAL(%s, %s, %s, %s)", base, arg1, arg2, arg3)


def IsStatement(G):
    for left in G.l("IsStatement", "left"):
        for right in G.l("IsStatement", "right"):
            yield S("%s IS %s", left, right)


def NotStatement(G):
    for op in G.l("NotStatement", "ops"):
        for i in G.g("inputConversions"):
            yield S("%s %s", op, i)


def LogicalStatement(G):
    for op in G.l("LogicalStatement", "ops"):
        for left in G.g("inputConversions"):
            for right in G.g("inputConversions"):
                yield S("%s %s %s", left, op, right)


def TupleComparisons(G):
    tuples = []
    perm(G.l("TupleComparisons", "elems"), lambda t: tuples.append("(" + ", ".join(t) + ")"))
    for op in G.l("TupleComparisons", "operators"):
        for i in range(len(tuples)):
            for j in range(len(tuples)):
                yield S("%s %s %s", tuples[i], op, tuples[j])


def Comparisons(G):
    for op in G.l("Comparisons", "operators"):
        for left in G.g("inputComparisonElement"):
            for right in G.g("inputComparisonElement"):
                yield S("%s %s %s", left, op, right)
        for left in G.g("inputConversions"):
            for right in G.g("inputConversions"):
                yield S("%s %s %s", left, op, right)


def InStatement(G):
    roots = G.l("InStatement", "roots")
    for inputs in gen_subsets(roots, 3):
        if not can_compare(*inputs):
            continue
        yield S("%s IN (%s, %s)", inputs[0], inputs[1], inputs[2])
        yield S("%s IN (%s, %s)", inputs[2], inputs[1], inputs[0])
        yield S("%s IN (%s, %s)", inputs[1], inputs[0], inputs[2])
        yield S("%s IN (%s, %s, %s)", inputs[0], inputs[1], inputs[2], inputs[0])
        yield S("%s NOT IN (%s, %s)", inputs[0], inputs[1], inputs[2])
        yield S("%s NOT IN (%s, %s)", inputs[2], inputs[1], inputs[0])
        yield S("%s NOT IN (%s, %s)", inputs[1], inputs[0], inputs[2])
        yield S("%s NOT IN (%s, %s, %s)", inputs[0], inputs[1], inputs[2], inputs[0])


def FnField(G):
    strings = G.g("inputStrings")
    radians = G.g("radianInputs")
    bitwise = G.g("inputBitwise")
    for a, b, c in ((strings, strings, strings), (radians, radians, radians), (strings, radians, strings), (bitwise, bitwise, bitwise)):
        for s1 in a:
            for s2 in b:
                for s3 in c:
                    yield S("FIELD(%s, %s, %s)", s1, s2, s3)
    yield from G.l("FnField", "mysqlDocSamples")


def FnElt(G):
    strings = G.g("inputStrings")
    bitwise = G.g("inputBitwise")
    for s1 in strings:
        for n in bitwise:
            yield S("ELT(%s, %s)", n, s1)
    for s1 in strings:
        for s2 in strings:
            for n in bitwise:
                yield S("ELT(%s, %s, %s)", n, s1, s2)
    for s1 in strings:
        for s2 in strings:
            for s3 in strings:
                for n in G.l("FnElt", "validIndex"):
                    yield S("ELT(%s, %s, %s, %s)", n, s1, s2, s3)
    yield from G.l("FnElt", "mysqlDocSamples")


def FnInsert(G):
    for s in G.g("insertStrings"):
        for ns in G.g("insertStrings"):
            for length in G.g("inputBitwise"):
                for p in G.g("inputBitwise"):
                    yield S("INSERT(%s, %s, %s, %s)", s, p, length, ns)
    yield from G.l("FnInsert", "mysqlDocSamples")


def FnLower(G):
    for value in G.g("inputStrings"):
        yield S("LOWER(%s)", value)
        yield S("LCASE(%s)", value)


def FnUpper(G):
    for value in G.g("inputStrings"):
        yield S("UPPER(%s)", value)
        yield S("UCASE(%s)", value)


def FnCharLength(G):
    for value in G.g("inputStrings"):
        yield S("CHAR_LENGTH(%s)", value)
        yield S("CHARACTER_LENGTH(%s)", value)


def FnLength(G):
    for value in G.g("inputStrings"):
        yield S("LENGTH(%s)", value)
        yield S("OCTET_LENGTH(%s)", value)


def FnBitLength(G):
    for value in G.g("inputStrings"):
        yield S("BIT_LENGTH(%s)", value)


def FnAscii(G):
    for value in G.g("inputStrings"):
        yield S("ASCII(%s)", value)


def FnReverse(G):
    for value in G.g("inputStrings"):
        yield S("REVERSE(%s)", value)


def FnSpace(G):
    for c in G.l("FnSpace", "counts"):
        yield S("SPACE(%s)", c)


def FnOrd(G):
    for value in G.g("inputStrings"):
        yield S("ORD(%s)", value)


def FnRepeat(G):
    for value in G.g("inputStrings"):
        for cnt in G.l("FnRepeat", "counts"):
            yield S("REPEAT(%s, %s)", value, cnt)


def FnLeft(G):
    for value in G.g("inputStrings"):
        for cnt in G.l("FnLeft", "counts"):
            yield S("LEFT(%s, %s)", value, cnt)


def FnLpad(G):
    for value in G.g("inputStrings"):
        for cnt in G.l("FnLpad", "counts"):
            for pad in G.g("inputStrings"):
                yield S("LPAD(%s, %s, %s)", value, cnt, pad)


def FnRight(G):
    for value in G.g("inputStrings"):
        for cnt in G.l("FnRight", "counts"):
            yield S("RIGHT(%s, %s)", value, cnt)


def FnRpad(G):
    for value in G.g("inputStrings"):
        for cnt in G.l("FnRpad", "counts"):
            for pad in G.g("inputStrings"):
                yield S("RPAD(%s, %s, %s)", value, cnt, pad)


def FnLTrim(G):
    for value in G.g("inputTrimStrings"):
        yield S("LTRIM(%s)", value)


def FnRTrim(G):
    for value in G.g("inputTrimStrings"):
        yield S("RTRIM(%s)", value)


def FnTrim(G):
    trims = G.g("inputTrimStrings")
    modes = G.l("FnTrim", "modes")
    for value in trims:
        yield S("TRIM(%s)", value)
    for value in trims:
        for mode in modes:
            yield S("TRIM(%s FROM %s)", mode, value)
    for value in trims:
        for pat in trims:
            yield S("TRIM(%s FROM %s)", pat, value)
            for mode in modes:
                yield S("TRIM(%s %s FROM %s)", mode, pat, value)


def FnSubstr(G):
    yield from G.l("FnSubstr", "mysqlDocSamples")
    for value in G.g("inputStrings"):
        for i in G.g("radianInputs"):
            yield S("SUBSTRING(%s, %s)", value, i)
            for j in G.g("radianInputs"):
                yield S("SUBSTRING(%s, %s, %s)", value, i, j)


def FnLocate(G):
    yield from G.l("FnLocate", "mysqlDocSamples")
    for substr in G.g("locateStrings"):
        for value in G.g("locateStrings"):
            yield S("LOCATE(%s, %s)", substr, value)
            yield S("INSTR(%s, %s)", value, substr)
            yield S("POSITION(%s IN %s)", value, substr)
            for i in G.g("radianInputs"):
                yield S("LOCATE(%s, %s, %s)", substr, value, i)


def FnReplace(G):
    yield from G.l("FnReplace", "cases")
    strings = G.g("inputStrings")
    for substr in strings:
        for value in strings:
            for i in strings:
                yield S("REPLACE(%s, %s, %s)", substr, value, i)


def FnConcat(G):
    strings = G.g("inputStrings")
    conversions = G.g("inputConversions")
    for value in strings:
        yield S("CONCAT(%s)", value)
    for str1 in conversions:
        for str2 in conversions:
            yield S("CONCAT(%s, %s)", str1, str2)
    for str1 in strings:
        for str2 in strings:
            for str3 in strings:
                yield S("CONCAT(%s, %s, %s)", str1, str2, str3)


def FnConcatWs(G):
    strings = G.g("inputStrings")
    conversions = G.g("inputConversions")
    for value in strings:
        yield S("CONCAT_WS(%s, NULL)", value)
    for a, b, c in ((conversions, strings, strings), (strings, conversions, strings), (strings, strings, conversions)):
        for str1 in a:
            for str2 in b:
                for str3 in c:
                    yield S("CONCAT_WS(%s, %s, %s)", str1, str2, str3)


def FnChar(G):
    yield from G.l("FnChar", "mysqlDocSamples")
    for i1 in G.g("radianInputs"):
        for i2 in G.g("inputBitwise"):
            for i3 in G.g("inputConversions"):
                yield S("CHAR(%s, %s, %s)", i1, i2, i3)


def FnHex(G):
    for source in (G.g("inputStrings"), G.g("inputConversions"), G.g("inputBitwise")):
        for value in source:
            yield S("hex(%s)", value)


def FnUnhex(G):
    inputs = G.l("FnUnhex", "inputs")
    for input_ in G.g("inputConversions"):
        inputs.append("'" + input_.encode("utf-8").hex() + "'")
    for lhs in inputs:
        yield S("UNHEX(%s)", lhs)


def FnNow(G):
    yield from G.l("FnNow", "fns")


def FnInfo(G):
    yield from G.l("FnInfo", "fns")


def _date_format_string(G):
    return "".join("%" + c + " " for c in G.date_format_chars())


def FnDateFormat(G):
    date_format = _date_format_string(G)
    for d in G.g("inputConversions"):
        yield S("DATE_FORMAT(%s, %q)", d, date_format)


def FnConvertTz(G):
    zones = G.l("FnConvertTz", "timezoneInputs")
    for num1 in G.g("inputConversions"):
        for tz_from in zones:
            for tz_to in zones:
                yield S("CONVERT_TZ(%s, '%s', '%s')", num1, tz_from, tz_to)


def _each_conversion(G, *templates):
    for d in G.g("inputConversions"):
        for template in templates:
            yield S(template, d)


def FnDate(G):
    yield from _each_conversion(G, "DATE(%s)")


def FnDayOfMonth(G):
    yield from _each_conversion(G, "DAYOFMONTH(%s)", "DAY(%s)")


def FnDayOfWeek(G):
    yield from _each_conversion(G, "DAYOFWEEK(%s)")


def FnDayOfYear(G):
    yield from _each_conversion(G, "DAYOFYEAR(%s)")


def FnFromUnixtime(G):
    date_format = _date_format_string(G)
    for d in G.g("inputConversions"):
        yield S("FROM_UNIXTIME(%s)", d)
        yield S("FROM_UNIXTIME(%s, %q)", d, date_format)


def FnHour(G):
    yield from _each_conversion(G, "HOUR(%s)")


def FnMakedate(G):
    for y in G.g("inputConversions"):
        for d in G.g("inputConversions"):
            yield S("MAKEDATE(%s, %s)", y, d)


def FnMaketime(G):
    for h in G.g("inputConversions"):
        if not make_time_valid_hours(h):
            continue
        for m in G.l("FnMaketime", "minutes"):
            for s in G.g("inputConversions"):
                yield S("MAKETIME(%s, %s, %s)", h, m, s)


def FnMicroSecond(G):
    yield from _each_conversion(G, "MICROSECOND(%s)")


def FnMinute(G):
    yield from _each_conversion(G, "MINUTE(%s)")


def FnMonth(G):
    yield from _each_conversion(G, "MONTH(%s)")


def FnMonthName(G):
    yield from _each_conversion(G, "MONTHNAME(%s)")


def FnLastDay(G):
    yield from _each_conversion(G, "LAST_DAY(%s)")
    for d in G.l("FnLastDay", "dates"):
        yield S("LAST_DAY(%s)", d)


def FnToDays(G):
    yield from _each_conversion(G, "TO_DAYS(%s)")
    for d in G.l("FnToDays", "dates"):
        yield S("TO_DAYS(%s)", d)


def FnFromDays(G):
    yield from _each_conversion(G, "FROM_DAYS(%s)")
    for d in G.l("FnFromDays", "days"):
        yield S("FROM_DAYS(%s)", d)


def FnSecToTime(G):
    yield from _each_conversion(G, "SEC_TO_TIME(%s)")
    yield from G.l("FnSecToTime", "mysqlDocSamples")


def FnTimeToSec(G):
    yield from _each_conversion(G, "TIME_TO_SEC(%s)")
    for t in G.l("FnTimeToSec", "time"):
        yield S("TIME_TO_SEC(%s)", t)


def FnToSeconds(G):
    yield from _each_conversion(G, "TO_SECONDS(%s)")
    for t in G.l("FnToSeconds", "timeInputs"):
        yield S("TO_SECONDS(%s)", t)
    yield from G.l("FnToSeconds", "mysqlDocSamples")


def FnQuarter(G):
    yield from _each_conversion(G, "QUARTER(%s)")


def FnSecond(G):
    yield from _each_conversion(G, "SECOND(%s)")


def FnTime(G):
    yield from _each_conversion(G, "TIME(%s)")
    for d in G.l("FnTime", "times"):
        yield S("TIME(%s)", d)


def FnUnixTimestamp(G):
    yield "UNIX_TIMESTAMP()"
    yield from _each_conversion(G, "UNIX_TIMESTAMP(%s)", "UNIX_TIMESTAMP(%s) + 1")


def FnWeek(G):
    for i in range(16):
        for d in G.g("inputConversions"):
            yield S("WEEK(%s, %d)", d, i)
    yield from _each_conversion(G, "WEEK(%s)")


def FnWeekDay(G):
    yield from _each_conversion(G, "WEEKDAY(%s)")


def FnWeekOfYear(G):
    yield from _each_conversion(G, "WEEKOFYEAR(%s)")


def FnYear(G):
    yield from _each_conversion(G, "YEAR(%s)")


def FnYearWeek(G):
    for i in range(8):
        for d in G.g("inputConversions"):
            yield S("YEARWEEK(%s, %d)", d, i)
    yield from _each_conversion(G, "YEARWEEK(%s)")


def FnPeriodAdd(G):
    for p in G.g("inputBitwise"):
        for m in G.g("inputBitwise"):
            yield S("PERIOD_ADD(%s, %s)", p, m)
    for p in G.g("inputPeriods"):
        for m in G.g("inputBitwise"):
            yield S("PERIOD_ADD(%s, %s)", p, m)
    yield from G.l("FnPeriodAdd", "mysqlDocSamples")


def FnPeriodDiff(G):
    for p1 in G.g("inputBitwise"):
        for p2 in G.g("inputBitwise"):
            yield S("PERIOD_DIFF(%s, %s)", p1, p2)
    for p1 in G.g("inputPeriods"):
        for p2 in G.g("inputPeriods"):
            yield S("PERIOD_DIFF(%s, %s)", p1, p2)
    yield from G.l("FnPeriodDiff", "mysqlDocSamples")


def _each_ip(G, *templates):
    for d in G.g("ipInputs"):
        for template in templates:
            yield S(template, d)


def FnInetAton(G):
    yield from _each_ip(G, "INET_ATON(%s)")


def FnInetNtoa(G):
    yield from _each_ip(G, "INET_NTOA(%s)", "INET_NTOA(INET_ATON(%s))")


def FnInet6Aton(G):
    yield from _each_ip(G, "INET6_ATON(%s)")


def FnInet6Ntoa(G):
    yield from _each_ip(G, "INET6_NTOA(%s)", "INET6_NTOA(INET6_ATON(%s))")


def FnIsIPv4(G):
    yield from _each_ip(G, "IS_IPV4(%s)")


def FnIsIPv4Compat(G):
    yield from _each_ip(G, "IS_IPV4_COMPAT(%s)", "IS_IPV4_COMPAT(INET6_ATON(%s))")


def FnIsIPv4Mapped(G):
    yield from _each_ip(G, "IS_IPV4_MAPPED(%s)", "IS_IPV4_MAPPED(INET6_ATON(%s))")


def FnIsIPv6(G):
    yield from _each_ip(G, "IS_IPV6(%s)")


def FnBinToUUID(G):
    for d in G.g("uuidInputs"):
        yield S("BIN_TO_UUID(%s)", d)
    for d in G.g("uuidInputs"):
        for a in G.l("FnBinToUUID", "args"):
            yield S("BIN_TO_UUID(%s, %s)", d, a)


def FnIsUUID(G):
    for d in G.g("uuidInputs"):
        yield S("IS_UUID(%s)", d)


def FnUUID(G):
    yield "LENGTH(UUID())"
    yield "COLLATION(UUID())"
    yield "IS_UUID(UUID())"
    yield "LENGTH(UUID_TO_BIN(UUID())"


def FnUUIDToBin(G):
    for d in G.g("uuidInputs"):
        yield S("UUID_TO_BIN(%s)", d)
    for d in G.g("uuidInputs"):
        for a in G.l("FnUUIDToBin", "args"):
            yield S("UUID_TO_BIN(%s, %s)", d, a)


def DateMath(G):
    yield from G.l("DateMath", "mysqlDocSamples")
    for d in G.l("DateMath", "dates"):
        for i in G.g("inputIntervals"):
            for v in G.l("DateMath", "intervalValues"):
                yield S("DATE_ADD(%s, INTERVAL %s %s)", d, v, i)
                yield S("DATE_SUB(%s, INTERVAL %s %s)", d, v, i)
                yield S("TIMESTAMPADD(%v, %s, %s)", i, v, d)


def RegexpLike(G):
    yield from G.l("RegexpLike", "mysqlDocSamples")
    for i in G.g("regexInputs"):
        for p in G.g("regexInputs"):
            yield S("%s REGEXP %s", i, p)
            yield S("%s NOT REGEXP %s", i, p)
            for m in G.g("regexMatchStrings"):
                yield S("REGEXP_LIKE(%s, %s, %s)", i, p, m)


def RegexpInstr(G):
    yield from G.l("RegexpInstr", "mysqlDocSamples")


def RegexpSubstr(G):
    yield from G.l("RegexpSubstr", "mysqlDocSamples")


def RegexpReplace(G):
    yield from G.l("RegexpReplace", "mysqlDocSamples")


INPUT_PI = "314159265358979323846264338327950288419716939937510582097494459"

FAMILIES = {
    name: obj
    for name, obj in globals().items()
    if inspect.isgeneratorfunction(obj) and not name.startswith("_")
}
# Families whose literals live in helper functions shared by several families.
SHARED_HELPERS = {
    "_unary_radian_bitwise": _unary_radian_bitwise,
    "_binary_radian_bitwise": _binary_radian_bitwise,
    "_each_conversion": _each_conversion,
    "_each_ip": _each_ip,
    "_date_format_string": _date_format_string,
}


# ---------------------------------------------------------------------------
# Verification of the port against the Go source
# ---------------------------------------------------------------------------


def _python_constants(function, seen=None):
    seen = set() if seen is None else seen
    constants = set()
    stack = [function.__code__]
    while stack:
        code = stack.pop()
        if code in seen:
            continue
        seen.add(code)
        for constant in code.co_consts:
            if isinstance(constant, str):
                constants.add(constant)
            elif isinstance(constant, (tuple, frozenset)):
                stack.extend(item for item in constant if hasattr(item, "co_consts"))
                constants.update(item for item in constant if isinstance(item, str))
            elif hasattr(constant, "co_consts"):
                stack.append(constant)
    return constants


def verify_port(G, complete=True):
    """Raise ValueError if the port does not match the parsed Go generator.

    ``complete`` also requires every ported family to be listed in ``Cases``
    (disabled only for tests over a partial synthetic source).
    """
    problems = []
    for name in G.case_names:
        if name not in FAMILIES:
            problems.append(f"family {name} is not ported")
    for name in FAMILIES if complete else ():
        if name not in G.case_names:
            problems.append(f"ported family {name} is no longer in Cases")
    helper_constants = set()
    for helper in SHARED_HELPERS.values():
        helper_constants |= _python_constants(helper)
    for name in G.case_names:
        if name not in FAMILIES:
            continue
        constants = _python_constants(FAMILIES[name]) | helper_constants
        for literal in G.function_literals(name):
            if literal not in constants and literal not in MULTI_COMPARISON_SKIP_MARKERS:
                problems.append(f"{name}: Go literal {literal!r} is missing from the port")
    if problems:
        raise ValueError("Vitess port verification failed:\n  " + "\n  ".join(problems))


def check_sources(directory, allow_drift):
    drift = []
    for name, expected in FILE_HASHES.items():
        actual = hashlib.sha256((pathlib.Path(directory) / name).read_bytes()).hexdigest()
        if actual != expected:
            drift.append(f"{name}: {actual} != pinned {expected}")
    if drift and not allow_drift:
        raise SystemExit("Vitess testcases differ from the ported revision:\n  " + "\n  ".join(drift))
    return drift


# ---------------------------------------------------------------------------
# Enumeration, sampling and output
# ---------------------------------------------------------------------------


def enumerate_family(G, name):
    """Yield ``(ordinal, expression, row)`` for one family."""
    for ordinal, item in enumerate(FAMILIES[name](G)):
        if isinstance(item, tuple):
            expression, row = item
        else:
            expression, row = item, None
        yield ordinal, expression, row


def candidate_for(name, ordinal, expression, row, line):
    if row is None:
        setup = []
        sql = "SELECT " + expression
    else:
        setup = [
            f"CREATE TABLE {VTEVAL_TABLE}(autopk int primary key auto_increment, column0 JSON)",
            f"INSERT INTO {VTEVAL_TABLE}(column0) VALUES ({quote_sql_string(row)})",
        ]
        sql = f"SELECT {expression} FROM {VTEVAL_TABLE}"
    return common.make_candidate(
        candidate_id=f"vitess/evalengine/{name}:{ordinal}",
        repo=REPO,
        revision=REVISION,
        path=f"{TESTCASES}/cases.go",
        line=line,
        license=LICENSE,
        group=f"vitess/evalengine/{name}",
        setup=setup,
        sql=[sql],
        ordered=True,
    )


def quote_sql_string(text):
    return "'" + text.replace("\\", "\\\\").replace("'", "''") + "'"


def allocate(sizes, target):
    """Water-fill ``target`` across families so small families are kept whole."""
    quotas = {}
    remaining = target
    pending = sorted(sizes.items(), key=lambda item: (item[1], item[0]))
    for index, (name, size) in enumerate(pending):
        share = remaining // (len(pending) - index)
        take = min(size, share)
        quotas[name] = take
        remaining -= take
    return quotas


def run(source_dir, output, target, allow_drift=False, write=True):
    source_dir = pathlib.Path(source_dir)
    testcases = source_dir / TESTCASES
    drift = check_sources(testcases, allow_drift)
    revision = common.check_revision(source_dir, REVISION, allow_mismatch=allow_drift)
    G = GoInputs(testcases)
    verify_port(G)

    report = common.Report(
        "Vitess evalengine import",
        {
            "repository": REPO,
            "revision": revision,
            "path": TESTCASES,
            "license": LICENSE,
            "selection": "all" if target == 0 else f"deterministic sample, target {target:,}",
        },
    )
    if drift:
        report.note("source drift accepted with --allow-drift: " + "; ".join(drift))

    family_rows = []
    unique = {}  # family -> list of (hash, ordinal, expression, row)
    seen = set()
    total = 0
    for name in G.case_names:
        line = G.functions[name][2]
        if name in EXCLUDED_FAMILIES:
            count = sum(1 for _ in FAMILIES[name](G))
            total += count
            report.filter(f"excluded family {name} ({EXCLUDED_FAMILIES[name]})", count)
            family_rows.append((name, count, 0, 0))
            continue
        items = []
        generated = 0
        for ordinal, expression, row in enumerate_family(G, name):
            generated += 1
            if expression in EXCLUDED_EXPRESSIONS:
                report.filter(f"{EXCLUDED_EXPRESSIONS[expression]} ({expression})")
                continue
            if not common.is_utf8_text(expression):
                report.filter("expression is not valid UTF-8")
                continue
            key = (expression, row)
            if key in seen:
                report.filter("duplicate expression")
                continue
            seen.add(key)
            digest = common.stable_hash(f"vitess/evalengine/{name}:{ordinal}")
            items.append((digest, ordinal, expression, row))
        total += generated
        unique[name] = (line, items)
        family_rows.append((name, generated, len(items), None))

    sizes = {name: len(items) for name, (_line, items) in unique.items()}
    available = sum(sizes.values())
    quotas = sizes if target == 0 or target >= available else allocate(sizes, target)

    candidates = []
    rows = []
    sampled_out = 0
    for name, generated, kept, _ in family_rows:
        if name not in unique:
            rows.append((name, generated, 0, 0))
            continue
        line, items = unique[name]
        quota = quotas[name]
        chosen = items if quota >= len(items) else heapq.nsmallest(quota, items)
        chosen = sorted(chosen, key=lambda item: item[1])
        for _digest, ordinal, expression, row in chosen:
            candidates.append(candidate_for(name, ordinal, expression, row, line))
        rows.append((name, generated, kept, len(chosen)))
        sampled_out += len(items) - len(chosen)

    report.count("Go families", len(G.case_names))
    report.count("expressions generated", total)
    report.count("unique eligible expressions", available)
    report.count("not selected by sample", sampled_out)
    report.count("candidates emitted", len(candidates))
    report.count("setup statements in candidates", sum(len(candidate["setup"]) for candidate in candidates))
    report.table("Families", ["family", "generated", "eligible", "emitted"], rows)
    report.table("Levels", ["level", "candidates"], common.level_histogram(candidates))
    report.note(
        "Expressions are generated by a Python port of the Vitess generator loops; lists are read "
        "from the Go source and format strings are cross-checked against it on every run."
    )
    report.note(
        "Sampling keeps small families whole and splits the remaining budget evenly; within a "
        "family the expressions with the smallest SHA-256 of their id are chosen, so a larger "
        "target is a superset of a smaller one and ids never depend on the target."
    )
    report.note(
        f"Families with a row schema ({', '.join(sorted(n for n in FAMILIES if n == 'JSONExtract'))}) "
        f"insert their row into `{VTEVAL_TABLE}` as setup, as the Vitess MySQL comparison harness does."
    )
    if write:
        common.write_candidates(output, SOURCE_NAME, candidates)
        text = report.write(output)
    else:
        text = report.render()
    return candidates, report, text


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--source", required=True, type=pathlib.Path, help="Vitess checkout")
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parents[1] / "candidates" / SOURCE_NAME,
    )
    parser.add_argument("--target", type=int, default=DEFAULT_TARGET, help="sample size (default %(default)s)")
    parser.add_argument("--all", action="store_true", help="emit every eligible expression")
    parser.add_argument("--allow-drift", action="store_true", help="accept a source that differs from the pinned port")
    args = parser.parse_args(argv)
    target = 0 if args.all else args.target
    if target < 0:
        parser.error("--target must be positive")
    candidates, _report, text = run(args.source, args.output, target, args.allow_drift)
    print(text)
    print(f"wrote {len(candidates):,} candidates to {args.output}")


if __name__ == "__main__":
    main()
