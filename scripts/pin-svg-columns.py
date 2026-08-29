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
        # Both ends pinned to UTF-8. Without it Python uses the host's locale
        # encoding, and on a Windows shell that is usually cp1252, which cannot
        # represent a single box-drawing character: the read raises, or worse,
        # a lossy round-trip writes an asset back with the table gone.
        before = path.read_text(encoding="utf-8")
        after, split = pin(before)
        path.write_text(after, encoding="utf-8")
        # Encoded, because these are byte counts and a box-drawing character is
        # three bytes to Python's one. Reporting `len` of the string understated
        # hero.svg by 2K and put a wrong figure in this commit's own pull request.
        print(
            f"{path}: split {split} mixed runs, "
            f"{len(before.encode('utf-8'))} -> {len(after.encode('utf-8'))} bytes"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:] or ["assets/hero.svg", "assets/lookout.svg"]))
