import os
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import import_vitess as vitess  # noqa: E402

CASES_GO = '''package testcases

var Cases = []TestCase{
	{Run: FnPi},
	{Run: FnSpace},
	{Run: TupleComparisons},
}

func FnPi(yield Query) {
	yield("PI()+0.000000000000000000", nil, false)
}

func FnSpace(yield Query) {
	counts := []string{
		"0",
		"'jhgjhg'",
	}

	for _, c := range counts {
		yield(fmt.Sprintf("SPACE(%s)", c), nil, false)
	}
}

func TupleComparisons(yield Query) {
	elems := []string{"NULL", "-1", "0", "1"}
	operators := []string{"=", "!=", "<=>", "<", "<=", ">", ">="}

	var tuples []string
	perm(elems, func(t []string) {
		tuples = append(tuples, "("+strings.Join(t, ", ")+")")
	})

	for _, op := range operators {
		for i := 0; i < len(tuples); i++ {
			for j := 0; j < len(tuples); j++ {
				yield(fmt.Sprintf("%s %s %s", tuples[i], op, tuples[j]), nil, false)
			}
		}
	}
}
'''

INPUTS_GO = '''package testcases

var inputBitwise = []string{
	"0", `'1.5'`,
	strconv.FormatUint(math.MaxUint64, 10),
	strconv.FormatInt(math.MinInt64, 10),
}

var radianInputs = []string{
	string(format.FormatFloat(math.Pi)),
	string(format.FormatFloat(math.MaxFloat64)),
	string(format.FormatFloat(math.SmallestNonzeroFloat32)),
	string(format.FormatFloat(math.SmallestNonzeroFloat64)),
}

var dateFormats = []struct {
	c    byte
	expr string
}{
	{'a', "LEFT(DAYNAME(d),3)"},
	{'%', ""},
}
'''


def write_sources(directory, cases=CASES_GO):
    directory = pathlib.Path(directory)
    (directory / "cases.go").write_text(cases)
    (directory / "inputs.go").write_text(INPUTS_GO)
    (directory / "helpers.go").write_text("package testcases\n")
    return directory


class GeneratorHelperTests(unittest.TestCase):
    def test_go_helpers(self):
        self.assertEqual(
            vitess.gen_subsets(["a", "b", "c", "d"], 2),
            [["a", "b"], ["a", "c"], ["a", "d"], ["b", "c"], ["b", "d"], ["c", "d"]],
        )
        permutations = []
        vitess.perm(["x", "y", "z"], permutations.append)
        self.assertEqual(len(permutations), 6)
        self.assertEqual(len({tuple(p) for p in permutations}), 6)
        self.assertEqual(permutations[0], ["x", "y", "z"])

    def test_bug_filters(self):
        self.assertFalse(vitess.can_compare("0x1", "1"))
        self.assertFalse(vitess.can_compare("18446744073709551615", "1", "-1"))
        self.assertTrue(vitess.can_compare("18446744073709551615", "1"))
        self.assertFalse(vitess.make_time_valid_hours("-9223372036854775808"))
        self.assertTrue(vitess.make_time_valid_hours("'1'"))
        self.assertTrue(vitess.make_time_valid_hours("18446744073709551615"))

    def test_go_formatting(self):
        self.assertEqual(vitess.S("DATE_FORMAT(%s, %q)", "d", "%a %% "), 'DATE_FORMAT(d, "%a %% ")')
        self.assertEqual(vitess.S("WEEK(%s, %d)", "x", 3), "WEEK(x, 3)")
        self.assertEqual(vitess.go_quote('a"b\\c\n'), '"a\\"b\\\\c\\n"')
        self.assertEqual(vitess.vitess_format_float(3.141592653589793), "3.141592653589793")
        self.assertEqual(vitess.vitess_format_float(1.7976931348623157e308), "1.7976931348623157e308")
        self.assertEqual(vitess.vitess_format_float(2.0**-149), "1.401298464324817e-45")
        self.assertEqual(vitess.vitess_format_float(5e-324), "5e-324")
        self.assertEqual(vitess.vitess_format_float(1e15), "1e15")
        self.assertEqual(vitess.vitess_format_float(123.0), "123")

    def test_allocation_keeps_small_families_whole(self):
        quotas = vitess.allocate({"small": 1, "medium": 10, "large": 100}, 30)
        self.assertEqual(quotas, {"small": 1, "medium": 10, "large": 19})


class PortVerificationTests(unittest.TestCase):
    def test_lists_come_from_go_source(self):
        with tempfile.TemporaryDirectory() as directory:
            G = vitess.GoInputs(write_sources(directory))
        self.assertEqual(G.case_names, ["FnPi", "FnSpace", "TupleComparisons"])
        self.assertEqual(G.g("inputBitwise"), ["0", "'1.5'", "18446744073709551615", "-9223372036854775808"])
        self.assertEqual(
            G.g("radianInputs"),
            ["3.141592653589793", "1.7976931348623157e308", "1.401298464324817e-45", "5e-324"],
        )
        self.assertEqual(G.date_format_chars(), ["a", "%"])
        self.assertEqual(list(vitess.FnSpace(G)), ["SPACE(0)", "SPACE('jhgjhg')"])
        tuples = list(vitess.TupleComparisons(G))
        self.assertEqual(len(tuples), 7 * 24 * 24)
        self.assertEqual(tuples[0], "(NULL, -1, 0, 1) = (NULL, -1, 0, 1)")
        vitess.verify_port(G, complete=False)
        with self.assertRaisesRegex(ValueError, "FnJSONKeys is no longer in Cases"):
            vitess.verify_port(G)

    def test_changed_go_literal_is_detected(self):
        with tempfile.TemporaryDirectory() as directory:
            G = vitess.GoInputs(write_sources(directory, CASES_GO.replace('"SPACE(%s)"', '"SPACES(%s)"')))
        with self.assertRaisesRegex(ValueError, "SPACES"):
            vitess.verify_port(G, complete=False)

    def test_unported_family_is_detected(self):
        cases = CASES_GO.replace("{Run: FnPi},", "{Run: FnPi},\n\t{Run: FnBrandNew},")
        with tempfile.TemporaryDirectory() as directory:
            G = vitess.GoInputs(write_sources(directory, cases))
        with self.assertRaisesRegex(ValueError, "FnBrandNew"):
            vitess.verify_port(G, complete=False)

    def test_candidates_are_setup_free_selects(self):
        candidate = vitess.candidate_for("FnPi", 0, "PI()+0.000000000000000000", None, 10)
        self.assertEqual(candidate["id"], "vitess/evalengine/FnPi:0")
        self.assertEqual(candidate["sql"], ["SELECT PI()+0.000000000000000000"])
        self.assertEqual(candidate["setup"], [])
        self.assertTrue(candidate["ordered"])
        with_row = vitess.candidate_for("JSONExtract", 3, "column0->>'$.a'", '{"a": "it\'s"}', 1)
        self.assertEqual(with_row["sql"], ["SELECT column0->>'$.a' FROM vteval_test"])
        self.assertEqual(with_row["setup"][1], "INSERT INTO vteval_test(column0) VALUES ('{\"a\": \"it''s\"}')")


SOURCE = os.environ.get("VITESS_SOURCE")


@unittest.skipUnless(SOURCE, "set VITESS_SOURCE to a Vitess checkout at the pinned revision")
class PinnedSourceTests(unittest.TestCase):
    """Cross-checks the port against counts derived by hand from the Go loops."""

    @classmethod
    def setUpClass(cls):
        directory = pathlib.Path(SOURCE) / vitess.TESTCASES
        vitess.check_sources(directory, allow_drift=False)
        cls.G = vitess.GoInputs(directory)
        vitess.verify_port(cls.G)

    def count(self, family):
        return sum(1 for _ in vitess.FAMILIES[family](self.G))

    def test_family_sizes(self):
        conversions, bitwise = 162, 48
        elements = bitwise + 12
        expected = {
            "Arithmetic": 7 * (conversions**2 + bitwise**2),
            "If": elements**3,
            "CaseExprWithPredicate": 5 * elements**2 + 10 * (elements * (elements - 1) * (elements - 2) // 6),
            "FnInsert": 18 * 18 * bitwise * bitwise + 3,
            "DateMath": 15 + 9 * 20 * 25 * 3,
            "FnMaketime": (conversions - 1) * 7 * conversions,
            "TupleComparisons": 7 * 24 * 24,
            "LargeDecimals": 2 * 126,
            "LargeIntegers": 2 * 125,
            "DecimalClamping": 63 * (sum(range(1, 18)) + 15 * 17),
            "FnWeek": 17 * conversions,
        }
        for family, size in expected.items():
            with self.subTest(family=family):
                self.assertEqual(self.count(family), size)

    def test_known_expressions(self):
        self.assertIn("CEIL(18446744073709551615)", set(vitess.FnCeil(self.G)))
        self.assertEqual(
            list(vitess.FnDateFormat(self.G))[0],
            'DATE_FORMAT(0, "%a %b %c %D %d %e %f %H %h %I %i %j %k %l %M %m %p %r %S %s %T %U %u %V %v %W %w %X %x %Y %y %% ")',
        )


if __name__ == "__main__":
    unittest.main()
