#!/usr/bin/env python3
"""Split every run in a terminal-recording SVG at its font-fallback boundary.

`assets/hero.svg` and `assets/lookout.svg` place a whole row of characters with
one `textLength` and `lengthAdjust="spacingAndGlyphs"`. That pins the row's
total width and nothing else: inside the run a renderer distributes the
correction in PROPORTION to each glyph's natural advance. So the moment the
box-drawing characters resolve to a different font than the ASCII beside them,
which is what happens once the stack falls past SF Mono, a mixed row's
separators walk away from the pure-border row above it. The error accumulates
left to right, so the rightmost columns are the worst. Measured 2026-08-29 at
up to 33px, four columns, and visible in Firefox on the rendered README.

The fix is to never let the two classes share a run. Each maximal run of
same-class characters gets its own `x` and its own `textLength`, so a run is
homogeneous, proportional distribution inside it is uniform, and its start is a
coordinate rather than the end of whatever came before.

Placing every character individually also works and is worse: glyphs then draw
at their natural width at 8.4px intervals, so the horizontal rules come out
dashed. Keeping `textLength` on a homogeneous run stretches the glyphs to meet.

Idempotent, and a no-op on a row that is already homogeneous. Run it again
after regenerating either asset.
"""

from __future__ import annotations

import html
import re
import sys
from pathlib import Path

TSPAN = re.compile(
    r'<tspan\b(?P<before>[^>]*?)'
    r'\stextLength="(?P<len>[\d.]+)"'
    r'\slengthAdjust="spacingAndGlyphs"'
    r'(?P<after>[^>]*)>(?P<text>[^<]*)</tspan>'
)
X_ATTR = re.compile(r'\sx="(?P<x>[-\d.]+)"')


def _fmt(value: float) -> str:
    """Trim a coordinate to the shortest form that still round-trips."""
    return f"{value:.2f}".rstrip("0").rstrip(".") or "0"


def _runs(chars: list[str]) -> list[tuple[int, int]]:
    """Maximal (offset, length) runs of characters that share a font.

    All ASCII shares one run, and every non-ASCII character gets a run of its
    own kind. Two different rules for two different problems.

    ASCII is grouped because a proportional fallback spaces the letters inside
    a cell unevenly and that does not matter: a cell's own edges are the
    characters either side of it, and those are pinned.

    Non-ASCII is split per character because a run of one repeated glyph is
    uniform in ANY font, while a mixed run like `\u250c\u2500\u2500\u252c` is uniform only if the
    fallback happens to give all three the same advance. Comic Sans is a font
    where it does not, and 3.8px of drift survived splitting on class alone.

    Keying on the character itself rather than on its Unicode block also means
    the rule needs no inventory of what the assets contain. Today that is the
    U+2500 box-drawing block plus one U+2026 ellipsis in lookout.svg; tomorrow
    it is whatever the next capture happens to print.
    """

    def key(char: str) -> object:
        return "ascii" if ord(char) < 128 else char

    spans: list[tuple[int, int]] = []
    start = 0
    for i in range(1, len(chars) + 1):
        if i == len(chars) or key(chars[i]) != key(chars[start]):
            spans.append((start, i - start))
            start = i
    return spans


def pin(svg: str) -> tuple[str, int]:
    split = 0

    def one(match: re.Match[str]) -> str:
        nonlocal split
        attrs = match["before"] + match["after"]
        origin_attr = X_ATTR.search(attrs)
        if origin_attr is None:
            return match[0]

        chars = list(html.unescape(match["text"]))
        if not chars:
            return match[0]

        origin = float(origin_attr["x"])
        cell = float(match["len"]) / len(chars)
        rest = X_ATTR.sub("", attrs, count=1).strip()
        rest = f" {rest}" if rest else ""

        spans = _runs(chars)
        if len(spans) > 1:
            split += 1
        return "".join(
            f'<tspan x="{_fmt(origin + offset * cell)}"'
            f' textLength="{_fmt(length * cell)}"'
            f' lengthAdjust="spacingAndGlyphs"{rest}>'
            f'{html.escape("".join(chars[offset:offset + length]), quote=False)}</tspan>'
            for offset, length in spans
        )

    return TSPAN.sub(one, svg), split


def main(paths: list[str]) -> int:
    for name in paths:
        path = Path(name)
        # Bytes on both ends, decoded and encoded here rather than by
        # `read_text`/`write_text`, which get two separate things wrong.
        #
        # ENCODING. Without an explicit one Python uses the host's locale, and
        # on a Windows shell that is usually cp1252. cp1252 does not REFUSE
        # UTF-8 box-drawing bytes, which would at least be loud; it decodes
        # them, into three mojibake characters per glyph. The script then sees
        # 12 characters where the row has 4, divides every `textLength` by
        # three times too many, and writes a structurally corrupt SVG with no
        # error anywhere. Verified: `"┌─┼…".encode("utf-8").decode("cp1252")`
        # returns 12 characters and re-encodes to the identical bytes, so even
        # a byte-for-byte diff of the file would not catch it.
        #
        # NEWLINES. `write_text` defaults to `newline=None`, which translates
        # every "\n" to `os.linesep`. On Windows that rewrites both assets with
        # CRLF as a side effect of a run that was meant to change x
        # coordinates. `write_bytes` performs no translation.
        raw = path.read_bytes()
        after, split = pin(raw.decode("utf-8"))
        written = after.encode("utf-8")
        path.write_bytes(written)
        # These are the bytes read and the bytes written, not a character count
        # standing in for them. A box-drawing character is one character and
        # three bytes, so `len` of the string understated hero.svg by 2K and put
        # a wrong figure in this pull request before it was caught.
        print(f"{path}: split {split} mixed runs, {len(raw)} -> {len(written)} bytes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:] or ["assets/hero.svg", "assets/lookout.svg"]))
