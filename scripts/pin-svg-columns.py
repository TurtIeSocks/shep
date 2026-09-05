#!/usr/bin/env python3
"""Split every run in a terminal-recording SVG at its font-fallback boundary.

`assets/hero.svg` and `assets/lookout.svg` pin a whole row's width with one
`textLength` and `lengthAdjust="spacingAndGlyphs"`, so a renderer distributes
the correction proportionally to each glyph's advance. Once
box-drawing characters fall back to a different font than the ASCII beside
them, a mixed row's separators drift from the row above, worst at the right
edge.

Splits each maximal same-class run into its own `tspan` and `textLength` so
distribution stays uniform inside it. Placing every character individually
avoids the drift too, but leaves the horizontal rules dashed since nothing
stretches to fill the cell.

Idempotent: a no-op on an already homogeneous row. Run again after
regenerating either asset.
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
    """Trim a coordinate to the shortest form that does not lose it.

    Six decimals, not two: at two, a run whose share of a small `textLength`
    falls under 0.005 rounds to "0" and renders as a zero-width run.
    """
    return f"{value:.6f}".rstrip("0").rstrip(".") or "0"


def _runs(chars: list[str]) -> list[tuple[int, int]]:
    """Maximal (offset, length) runs of characters that share a font.

    ASCII is grouped as one run: fallback spacing inside a cell doesn't
    matter, since a cell's own edges are the pinned characters either side of
    it. Non-ASCII is split per character, since a run of mixed glyphs is
    uniform only if the fallback gives them all the same advance.

    Keys on the character itself, not its Unicode block, so the rule needs no
    inventory of what an asset contains.
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
        # Bytes on both ends, not read_text/write_text: an implicit locale
        # encoding (cp1252 on Windows) decodes UTF-8 box-drawing bytes into
        # mojibake instead of refusing them, so the script divides
        # `textLength` by the wrong character count with no error.
        #
        # `write_text`'s default `newline=None` also rewrites both assets to
        # CRLF on Windows; `write_bytes` performs no translation.
        raw = path.read_bytes()
        after, split = pin(raw.decode("utf-8"))
        written = after.encode("utf-8")
        path.write_bytes(written)
        # Bytes read/written, not a character count: a box-drawing character
        # is three bytes, so `len` of the string understates the real size.
        print(f"{path}: split {split} mixed runs, {len(raw)} -> {len(written)} bytes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:] or ["assets/hero.svg", "assets/lookout.svg"]))
