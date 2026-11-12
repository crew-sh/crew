#!/usr/bin/env bash
# Renders README.md into site/out/index.html with pandoc.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p site/out
cp site/style.css site/out/style.css
pandoc README.md -f gfm -t html5 -s --metadata pagetitle="crew: tools for running many AI coding agents on one machine" --css style.css \
  -V include-before='<main><div class="top"><a href="https://github.com/crew-sh/crew">GitHub</a><a href="https://github.com/crew-sh/crew/releases">Releases</a></div>' \
  -V include-after='</main>' -o site/out/index.html
