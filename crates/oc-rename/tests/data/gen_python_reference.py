"""Reference outputs of rename.new_name for the Rust port (differential test fixture)."""
import json, re, sys
from pathlib import Path
from orchard_commander import rename, vfs
from orchard_commander.rename import Item, Rule, MaskError, MetadataPending, MissingMetadata

NAMES = ["IMG_20260307_beach.JPG", "a.tar.gz", ".bashrc", "no_ext", "Ünïcødé straße.Txt", "hello WORLD-again.txt",
         "x", "trailing.", "..x", "日本語ファイル.jpeg", "ǆemal ǅ.TXT", "ΣΟΦΟΣ.md", "a b  c.d e", "Straße", "v1.2",
         "x*y_z-1.2-3.tXt", "émoji 😀 file.PNG", "İstanbul.txt",
         "İıIi kKK sSſ ßẞ σςΣ θϑϴ µμΜ ǅǆǄ ιͅιΙ ﬅﬆ.txt"]
MASKS = ["[N]", "[N1-3]", "[N5,8]", "[N14-]", "[N-5-]", "[N-5,2]", "[N-5-4]", "[N5--7]", "[N1]", "[2-4]", "[N0-3]",
         "[N2,]", "[N2,-3]", "[N-100-]", "[N100]", "[N3-1]", "[N-2--5]", "[N2-0]", "[N-3-0]", "[N-0]", "[N2-5-]",
         "[N 2]", "[N2x]", "[N+2]", "[E]", "[E1-2]", "[E-1]", "[P]", "[G]", "[P1-3]", "[G-2-]", "[U][N][n]",
         "[L]X[N]", "[F][N]", "[F]my holiday [N14-]", "[[]x[]]", "[N", "a]b", "[X]", "[Cabc]", "[C]", "[C10+5:3]",
         "[C:4]", "[C+-2]", "[C-1]", "[C+]", "[C:]", "[C0]", "[C+0:2]", "[C5:0]", "[]", "[]]", "[[]", "[[", "]]",
         "[n]", "[U]", "[U]ab[L]CD[F]ef gh[n]IJ", "[N][N][E]", "[1-]", "[-3-]", "[-1]", "[=artist]", "[=artist:1-2]",
         "[=artist|Unknown]", "[=artist:0-2]", "[=nonsense]", "[=]", "[= artist]", "[=artist:x]", "[=title|a|b]",
         "[=artist:]", "[=track]-[N]", "[U][=artist|unk]", "[=artist:2,]", "x[=artist:-2-]", "[N]/[E]", "",
         "[F]o'neil mc-donald_x", "[F]ǆungla ßtraße"]
EXT_MASKS = ["[E]", "", "[L][E]", "[U][E]", "txt", "[E1]", "[E", "[N]", "[C:2]"]
SEARCHES = [("copy", ""), ("-|_", " "), ("-|_", "+|="), ("b", "x"), (".", "_"), ("A|B|C", "1|2"), ("||a", "1|2|3"),
            (r"(\d+)-(\d+)-(\d+)", "$3.$2.$1"), (r"(\w)(\w)", r"\2\1"), ("x*", "-"), ("^", ">"), ("$", "<"),
            (r"(?P<x>a)", r"[\g<x>\g<0>]"), (r"\s+", " "), (r"(e)", r"$1$1\1"), ("ß", "ss"), ("S", "z"), ("é", "E"),
            (r"(a)", r"\2"), (r"a", r"\q"), (r"a", "x\\"), ("(", "x"), (r"[aeiou]", ""), (r"(?i)A", "4"),
            (r"\.", "_"), (r"(.)\1", "<$1>"), (r"(?<=a)b", "B"), ("i", "I"), ("$0", "z"), (r"a??", "-"),
            ("[ix]", "_"), ("(?P<si>i)", "<\\g<si>>"), ("ı", "_"), ("İ", "_")] + [(c, "_") for c in "kKKsSſßẞσςΣθϑϴµμΜǅǆǄιͅιΙﬅﬆ"]
CASES = ["unchanged", "lower", "upper", "first", "words"]
META = {"artist": "AC-DC", "title": "Tone", "track": "03"}

def item(name, is_dir, meta):
    return Item(vfs.local(Path("/nowhere") / name), name, is_dir, 1772874304.0, "Holiday", "Photos", meta=meta)

def outcome(it, index, rule):
    try:
        return {"ok": rename.new_name(it, index, rule)}
    except MetadataPending:
        return {"pending": True}
    except MissingMetadata as e:
        return {"missing": str(e)}
    except MaskError as e:
        return {"mask_error": str(e), "pos": e.position}
    except re.error as e:
        try:
            re.compile(rule.search)
        except re.error:
            return {"pattern_error": True}
        return {"search_error": str(e)}
    except IndexError as e:
        return {"search_error": str(e).strip("'\"")}

def emit(cases, name, is_dir, meta, index, **kw):
    rule = Rule(**kw)
    cases.append({"name": name, "is_dir": is_dir, "meta": meta, "index": index, "rule": kw,
                  "expect": outcome(item(name, is_dir, meta), index, rule)})

cases = []
for name in NAMES:
    for is_dir in (False, True):
        for mask in MASKS:
            for meta in ((None, META, {}) if "[=" in mask else (None,)):
                emit(cases, name, is_dir, meta, 2, name_mask=mask)
        for ext in EXT_MASKS:
            emit(cases, name, is_dir, None, 0, ext_mask=ext)
    for search, replace in SEARCHES:
        for regex in (False, True):
            for cs in (False, True):
                if regex and not cs and "\\1" in search and "ſ" in name:
                    continue  # case-insensitive back-references: Unicode folding in Rust (documented)
                emit(cases, name, False, None, 0, search=search, replace=replace, regex=regex, case_sensitive=cs)
    for case in CASES:
        emit(cases, name, False, None, 0, case=case)
        emit(cases, name, True, None, 0, case=case, name_mask="[F][N] x")
for start, step, digits in ((1, 1, 1), (100, 10, 4), (-5, 3, 3), (0, -1, 2), (7, 0, 0)):
    for index in range(4):
        for mask in ("[C]", "Img[C:4]", "[C10]", "[C+5]", "[C3+2:3]"):
            emit(cases, "x.jpg", False, None, index, name_mask=mask, counter_start=start, counter_step=step,
                 counter_digits=digits)
# Regular-expression syntax: classes, escapes, quantifiers, flags. Left out on purpose
# (documented deviations): what Python refuses but Rust's engines accept (\\z \\p{L} \\h
# (?<n>…) "{2023}" "[a--b]" …), the ASCII flag (?a) and \\N{name}.
REGEX_NAMES = ["Report {2023} final [v2].pdf", "#tag ~home's \"q\" a&b-c x{y}.txt", "2026-03-07 trip a]b:].jpg"]
REGEX_SEARCHES = [r"x{", r"\{(\d+)\}", r"a{2}", r"\d{2,}", r"\d{,2}", r"o\Z", r"\AR", r"[[]", r"[]]", r"\-",
                  r"\ ", r"\#", r"\&", r"\~", r"\'", r'\"', r"\/", r"\:", r"[a-z]+", r"\d+", r"(?i)abc",
                  r"(?x) a b [ ]", r"^(.*)$", r"(.*)", r".*", r"\b\w", r"(?=\.)", r"(?!a).", r"[^.]+", r"\.(\w+)$",
                  r"(\d{4})-(\d{2})", r"\s", r"[&]", r"[a&&b]", r"[~~]", r"[[:alpha:]]", r"\x41", r"\u00e9",
                  r"(?P<n>a)(?P=n)", r"(?:a)", r"\W+", r"[\w.]+", r"(a)?b", r"e$", r"\B", r"(?#c)a", r"\[",
                  r"\]", r"[\]]", r"[{}]", r"{", r"}", r"a{1,2}?", r"a*?", r"[^\W\d]", r"(?s).", r"(?m)^a",
                  r"\0", r"\101", r"[\101-\103]", r"[\d-]", r"[-a]", r"[a-]", r"[]-]", r"[\]\-\\]", r"(?u)\w",
                  r"[\x41-\x5a\t]", r"[z-a]", r"[a", r"[\d-a]", r"\8", r"$\d"]
for name in REGEX_NAMES:
    for search in REGEX_SEARCHES:
        for replace in ("_", r"<\g<0>>", "$1", "&", r"\\"):
            for cs in (False, True):
                emit(cases, name, False, None, 0, search=search, replace=replace, regex=True, case_sensitive=cs)
out = Path(sys.argv[1])
out.write_text("\n".join(json.dumps(c, ensure_ascii=False) for c in cases) + "\n", encoding="utf-8")
print(len(cases), "cases")
