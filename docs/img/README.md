# docs/img

`demo.gif` is generated, not committed by hand. `README.md` links it from the top of the
page, so it should exist before the repo is public.

Get the corpus once:

```sh
scripts/demo-corpus.sh cpython
```

Then either route:

**Screen recorder** (ScreenToGif, N-Studio, LICEcap, peek) — works everywhere, including
Windows. `scripts/record-demo.sh` types and runs the whole demo for you at a watchable
pace, so you only have to start and stop the recorder:

```sh
CORPUS=~/cpython scripts/record-demo.sh     # ~25s; SPEED=1.0 for slower pacing
```

**VHS** (headless, reproducible, needs ttyd + ffmpeg — easiest on Linux/macOS):

```sh
CORPUS=~/cpython vhs demo/demo.tape        # writes docs/img/demo.gif directly
```

Notes:

- **Clone the corpus somewhere short.** Result paths are absolute, so
  `/tmp/ripindex-corpora/cpython-v3.12.0/Doc/library/asyncio-subprocess.rst` wraps and
  swamps the frame. `~/cpython` keeps it readable. `record-demo.sh` warns if the path is
  long.
- Save as `docs/img/demo.gif` and keep it under a couple of megabytes, or the README
  becomes slow to load. Reducing the frame rate to 12-15fps usually does it.
