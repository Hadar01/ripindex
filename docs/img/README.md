# docs/img

**Nothing here is required.** The README leads with a real terminal transcript in a code
block, not an image, which was a deliberate choice:

- It renders everywhere, including on **crates.io**, which does not resolve relative image
  paths — an `![](docs/img/demo.gif)` link shows up broken there.
- It stays true with no maintenance, and it's copy-pasteable and diffable.
- It needs no recorder, no export step, and no binary blob in git history.

If you *do* want an animated demo later, everything to make one is already here and tested.

## Recording

Get the corpus once, then run the driver — it types and runs the whole demo at a watchable
pace, so you only start and stop the recorder. The tallest screen is 9 rows by 88 columns,
so a **100x20 terminal, capturing the whole window** fits it with margin.

```powershell
scripts\record-demo.ps1                 # -Speed 1.4 for slower pacing
```

```sh
CORPUS=~/cpython scripts/record-demo.sh   # bash syntax; not valid in PowerShell
```

## Converting whatever the recorder gave you

Most recorders don't hand you a GIF: N-Studio and ScreenToGif save a *project* (a zip of
PNG frames plus a delay JSON) and put GIF export behind a paid editor, and OBS gives you an
mp4. `scripts/make-gif.ps1` converts any of those with ffmpeg
(`winget install Gyan.FFmpeg`), preserving the recorder's own per-frame timing:

```powershell
scripts\make-gif.ps1 -InputPath Project.zip
scripts\make-gif.ps1 -InputPath recording.mp4 -Fps 12 -MaxWidth 1000
```

It accepts a project `.zip`, a folder of PNG frames, or a video. If the result is over 2 MB
it says which knobs to turn (`-Fps`, `-MaxWidth`, `-Colors`).

A plain screenshot is also a perfectly good option, and on Windows it's one keypress
(`Win+Shift+S`) with no minimum region and no export step.

## If you add one

Reference it with an **absolute** URL so it works on crates.io as well as GitHub:

```markdown
![ripindex demo](https://raw.githubusercontent.com/hadar01/ripindex/main/docs/img/demo.gif)
```

Keep it under ~2 MB. Recorder intermediates (project archives, frame dumps, mp4s) are
gitignored — only the finished image belongs in the repo.
