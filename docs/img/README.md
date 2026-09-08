# docs/img

`demo.gif` is generated, not committed by hand. `README.md` links it from the top of the
page, so it should exist before the repo is public.

Get the corpus once:

```sh
scripts/demo-corpus.sh cpython
```

Then either route:

**Screen recorder** (ScreenToGif, N-Studio, LICEcap, peek) — works everywhere. The
`record-demo` scripts type and run the whole demo for you at a watchable pace, so you only
start and stop the recorder. Roughly a 20-second recording.

PowerShell:

```powershell
scriptsecord-demo.ps1                      # -Speed 1.4 for slower pacing
scriptsecord-demo.ps1 -Corpus C:\codeepo
```

Bash (Git Bash, Linux, macOS) — note this is a *bash* invocation; `CORPUS=x cmd` is not
valid PowerShell syntax:

```sh
CORPUS=~/cpython scripts/record-demo.sh      # SPEED=1.0 for slower pacing
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
