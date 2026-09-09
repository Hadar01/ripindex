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
scripts
ecord-demo.ps1                      # -Speed 1.4 for slower pacing
scripts
ecord-demo.ps1 -Corpus C:\code
epo
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

### If your recorder gives you frames or a video instead of a GIF

Common: N-Studio and ScreenToGif save a *project* (a zip of PNG frames plus a
delay JSON) and put GIF export behind a paid editor; OBS gives you an mp4.
`scripts/make-gif.ps1` turns any of those into an optimised GIF with ffmpeg
(`winget install Gyan.FFmpeg`), preserving the recorder's own per-frame timing:

```powershell
scripts\make-gif.ps1 -InputPath docs\img\Project.zip
scripts\make-gif.ps1 -InputPath recording.mp4 -Fps 12 -MaxWidth 1000
```

It accepts a project `.zip`, a folder of PNG frames, or a video, and writes
`docs/img/demo.gif` by default. If the result is over 2 MB it tells you which
knobs to turn (`-Fps`, `-MaxWidth`, `-Colors`).

Notes:

- **Clone the corpus somewhere short.** Result paths are absolute, so
  `/tmp/ripindex-corpora/cpython-v3.12.0/Doc/library/asyncio-subprocess.rst` wraps and
  swamps the frame. `~/cpython` keeps it readable. `record-demo.sh` warns if the path is
  long.
- Save as `docs/img/demo.gif` and keep it under a couple of megabytes, or the README
  becomes slow to load. Reducing the frame rate to 12-15fps usually does it.
