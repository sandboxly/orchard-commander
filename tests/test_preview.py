from pathlib import Path

from orchard_commander import preview
from orchard_commander.preview import Kind


def test_detect_kinds(tmp_path: Path):
    (tmp_path / "a.py").write_text("print('hi')\n")
    (tmp_path / "r.md").write_text("# Title\n")
    (tmp_path / "x.bin").write_bytes(bytes(range(256)))
    (tmp_path / "e.txt").write_bytes(b"")
    (tmp_path / "p.png").write_bytes(b"not really")
    (tmp_path / "v.mkv").write_bytes(b"")
    (tmp_path / "s.flac").write_bytes(b"")
    (tmp_path / "noext").write_text("plain words")
    assert preview.detect(tmp_path) is Kind.DIRECTORY
    assert preview.detect(tmp_path / "a.py") is Kind.TEXT
    assert preview.detect(tmp_path / "r.md") is Kind.MARKDOWN
    assert preview.detect(tmp_path / "x.bin") is Kind.BINARY
    assert preview.detect(tmp_path / "e.txt") is Kind.EMPTY
    assert preview.detect(tmp_path / "p.png") is Kind.IMAGE  # by extension; decoding decides later
    assert preview.detect(tmp_path / "v.mkv") is Kind.VIDEO
    assert preview.detect(tmp_path / "s.flac") is Kind.AUDIO
    assert preview.detect(tmp_path / "noext") is Kind.TEXT
    assert preview.detect(tmp_path / "missing") is Kind.UNREADABLE


def test_looks_like_text_handles_split_utf8_and_latin1():
    assert preview.looks_like_text("héllo wörld".encode()[:-1])  # cut inside 'ö'... still text
    assert preview.looks_like_text("Grüße aus Köln".encode("latin-1"))
    assert not preview.looks_like_text(b"\x00\x01\x02binary")


def test_read_text_truncates(tmp_path: Path):
    f = tmp_path / "big.txt"
    f.write_text("x" * 100)
    content = preview.read_text(f, limit=10)
    assert content.text == "x" * 10 and content.truncated
    assert not preview.read_text(f).truncated


def test_hexdump_format():
    line = preview.hexdump(b"ABCDEFGHIJKLMNOPQ").splitlines()
    assert line[0] == "00000000  41 42 43 44 45 46 47 48  49 4a 4b 4c 4d 4e 4f 50  ABCDEFGHIJKLMNOP"
    assert line[1].startswith("00000010  51")


def test_directory_summary(tree: Path):
    text = preview.directory_summary(tree / "src", show_hidden=False)
    assert text.startswith("1 directory, 2 files")
    assert ".hidden" not in text
    assert ".hidden" in preview.directory_summary(tree / "src", show_hidden=True)
