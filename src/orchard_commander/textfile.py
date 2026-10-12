"""Text files for the editor: decode bytes into text and back without changing what you didn't edit.

`decode()` detects the encoding (byte-order marks, UTF-8, else Windows-1252 /
Latin-1) and the line ending style; `TextFile.encode()` writes the text back the
same way. Files with NUL bytes outside UTF-16 are treated as binary.
"""

from __future__ import annotations

import codecs
from dataclasses import dataclass

BOMS = [  # longest first: the UTF-32 LE mark starts with the UTF-16 LE one
    (codecs.BOM_UTF32_LE, "utf-32-le"), (codecs.BOM_UTF32_BE, "utf-32-be"),
    (codecs.BOM_UTF8, "utf-8"), (codecs.BOM_UTF16_LE, "utf-16-le"), (codecs.BOM_UTF16_BE, "utf-16-be"),
]
NAMES = {"utf-8": "UTF-8", "utf-16-le": "UTF-16 LE", "utf-16-be": "UTF-16 BE", "utf-32-le": "UTF-32 LE",
         "utf-32-be": "UTF-32 BE", "cp1252": "Windows-1252", "latin-1": "Latin-1"}


class BinaryFile(ValueError):
    """The bytes aren't text."""


@dataclass
class TextFile:
    text: str  # with "\n" line endings
    encoding: str = "utf-8"
    bom: bool = False
    newline: str = "\n"  # "\n", "\r\n" or "\r": what the file used (the most common one)

    @property
    def description(self) -> str:
        """"UTF-8 · LF" for the status line."""
        ending = {"\n": "LF", "\r\n": "CRLF", "\r": "CR"}[self.newline]
        return f"{NAMES.get(self.encoding, self.encoding)}{' with BOM' if self.bom else ''} · {ending}"

    def encode(self, text: str | None = None) -> bytes:
        """`text` (default: self.text) in this file's encoding, BOM and line endings."""
        text = self.text if text is None else text
        if self.newline != "\n":
            text = text.replace("\n", self.newline)
        data = text.encode(self.encoding)
        if self.bom:
            data = next(bom for bom, name in BOMS if name == self.encoding) + data
        return data


def decode(data: bytes) -> TextFile:
    """Raises BinaryFile for data that isn't text."""
    for bom, encoding in BOMS:
        if data.startswith(bom):
            try:
                text = data[len(bom):].decode(encoding)
            except UnicodeDecodeError as exc:
                raise BinaryFile(f"not valid {NAMES[encoding]}") from exc
            return _with_newlines(text, encoding, bom=True)
    if b"\0" in data:
        raise BinaryFile("contains NUL bytes")
    try:
        return _with_newlines(data.decode("utf-8"), "utf-8", bom=False)
    except UnicodeDecodeError:
        pass
    try:
        text = data.decode("cp1252")
    except UnicodeDecodeError:  # cp1252 leaves five bytes undefined
        return _with_newlines(data.decode("latin-1"), "latin-1", bom=False)
    return _with_newlines(text, "cp1252", bom=False)


def _with_newlines(text: str, encoding: str, *, bom: bool) -> TextFile:
    crlf = text.count("\r\n")
    cr = text.count("\r") - crlf
    lf = text.count("\n") - crlf
    newline = max((("\n", lf), ("\r\n", crlf), ("\r", cr)), key=lambda pair: pair[1])[0] if (lf or crlf or cr) else "\n"
    normalized = text.replace("\r\n", "\n").replace("\r", "\n")
    return TextFile(normalized, encoding, bom, newline)
