"""Run with: python3 .claude/scripts/test_po.py"""

import importlib.util
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("po", Path(__file__).with_name("po.py"))
po = importlib.util.module_from_spec(spec)
spec.loader.exec_module(po)

CATALOG = '''msgid ""
msgstr ""
"Language: it\\n"

#: a.qml:1
msgid "Done"
msgstr "Fatto"

#: a.qml:2
#, fuzzy
msgid "Send"
msgstr "Vecchio"

#: a.qml:3
msgid ""
"Two lines "
"joined"
msgstr ""

#: a.qml:4
msgid "Say \\"hi\\""
msgstr ""

#: a.qml:5
#, fuzzy, kde-format
msgctxt "@action:button"
msgid "Show %1 File"
msgid_plural "Show %1 Files"
msgstr[0] "Vecchio"
msgstr[1] "Vecchi"

#: a.qml:6
msgid "%1 day"
msgid_plural "%1 days"
msgstr[0] "%1 giorno"
msgstr[1] "%1 giorni"

#: a.qml:7
msgctxt "@action:button"
msgid "Clear"
msgstr ""

#: a.qml:8
msgctxt "@info:status the field is empty"
msgid "Clear"
msgstr ""

#~ msgid "Gone"
#~ msgstr "Andato"
'''


class PoTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        root = Path(self.dir.name)
        (root / "LINGUAS").write_text("it\n")
        (root / "it.po").write_text(CATALOG, encoding="utf-8")
        self.original = po.PO_DIR
        po.PO_DIR = root
        self.addCleanup(setattr, po, "PO_DIR", self.original)

    def test_pending_lists_fuzzy_and_untranslated_with_context_and_plural(self) -> None:
        self.assertEqual(
            po.pending("it"),
            [
                {"msgid": "Send"},
                {"msgid": "Two lines joined"},
                {"msgid": 'Say "hi"'},
                {"msgid": "Show %1 File", "context": "@action:button", "msgid_plural": "Show %1 Files", "forms": 2},
                {"msgid": "Clear", "context": "@action:button"},
                {"msgid": "Clear", "context": "@info:status the field is empty"},
            ],
        )

    def test_apply_fills_entries_by_context_clears_fuzzy_and_keeps_the_rest(self) -> None:
        items = po.pending("it")
        translations = ["Invia", "Due righe", 'Di\' "ciao"', ["Mostra %1 file", "Mostra %1 file"], "Svuota", "Vuoto"]
        result = po.apply({"it": [item | {"msgstr": msgstr} for item, msgstr in zip(items, translations)]})
        self.assertEqual(result, {"it": 6})
        text = (po.PO_DIR / "it.po").read_text(encoding="utf-8")
        self.assertNotIn("fuzzy", text)
        self.assertIn('msgstr "Fatto"', text)
        self.assertIn('msgstr "Invia"', text)
        self.assertIn('msgstr "Di\' \\"ciao\\""', text)
        self.assertIn('#, kde-format\nmsgctxt "@action:button"\nmsgid "Show %1 File"\nmsgid_plural "Show %1 Files"\nmsgstr[0] "Mostra %1 file"\nmsgstr[1] "Mostra %1 file"', text)
        self.assertIn('msgctxt "@action:button"\nmsgid "Clear"\nmsgstr "Svuota"', text)
        self.assertIn('msgctxt "@info:status the field is empty"\nmsgid "Clear"\nmsgstr "Vuoto"', text)
        self.assertIn('msgstr[1] "%1 giorni"', text)
        self.assertIn('#~ msgstr "Andato"\n', text)
        self.assertEqual(po.pending("it"), [])

    def test_escapes_round_trip(self) -> None:
        for text in ('Say "hi"', "a\\nb", "line\nnext", "tab\there", "back\\slash"):
            self.assertEqual(po.unquote([po.quote(text)]), text)

    def test_apply_writes_nothing_when_an_item_does_not_fit(self) -> None:
        before = (po.PO_DIR / "it.po").read_text(encoding="utf-8")
        send = {"msgid": "Send", "msgstr": "Invia"}
        plural = {"msgid": "Show %1 File", "context": "@action:button"}
        bad = [
            plural | {"msgstr": "Mostra %1 file"},  # a plural message needs a list
            plural | {"msgstr": ["Mostra %1 file"]},  # of 2 forms
            {"msgid": "Show %1 File", "msgstr": ["a", "b"]},  # it has a context
            {"msgid": "Clear", "msgstr": "Svuota"},  # no message without context
            {"msgid": "Send", "msgstr": ["Invia", "Invia"]},  # not a plural message
            {"msgid": "Send"},  # no msgstr
            "Send",  # not an object
        ]
        for item in bad:
            with self.assertRaises(ValueError):
                po.apply({"it": [send, item]})
        for translations in ({"it": {"Send": "Invia"}}, {"fr": [send]}):  # the old format; no catalog
            with self.assertRaises(ValueError):
                po.apply(translations)
        self.assertEqual((po.PO_DIR / "it.po").read_text(encoding="utf-8"), before)


if __name__ == "__main__":
    unittest.main()
