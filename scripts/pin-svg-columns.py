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

    ASCII and the U+2500 box-drawing block are the only two classes these
    assets contain, and they are exactly the pair that resolves to different
    fonts when the monospace stack falls through.
    """
    def key(char: str) -> object:
        # ASCII shares one run: a proportional fallback spaces the letters
        # inside a cell unevenly, but the cell's own edges are box characters
        # and those are pinned. Every box character gets a run of its own kind,
        # because a run of one repeated glyph is uniform in ANY font, while a
        # mixed `\u250c\u2500\u2500\u252c` is only uniform if the fallback gives all three the
        # same advance. Comic Sans is a font where it does not: 3.8px of drift
        # survived the ASCII split alone.
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
        before = path.read_text()
        after, split = pin(before)
        path.write_text(after)
        print(f"{path}: split {split} mixed runs, {len(before)} -> {len(after)} bytes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:] or ["assets/hero.svg", "assets/lookout.svg"]))
