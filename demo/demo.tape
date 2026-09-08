# VHS tape for the README's demo GIF: https://github.com/charmbracelet/vhs
#
#   go install github.com/charmbracelet/vhs@latest      (or: brew install vhs)
#   vhs demo/demo.tape
#
# Writes docs/img/demo.gif. Point CORPUS at a large tree you have locally -
# scripts/demo-corpus.sh clones a good one:
#
#   scripts/demo-corpus.sh cpython
#   CORPUS=/tmp/ripindex-corpora/cpython-v3.12.0 vhs demo/demo.tape

Output docs/img/demo.gif

Set Shell bash
Set FontSize 18
Set Width 1200
Set Height 620
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms
Set Padding 20

# Pre-build the index off-camera: the GIF should show what the tool feels like
# in daily use, which is the warm path. The first-run cost is in the README's
# benchmark table instead, where it can be stated honestly with a number.
Hide
Type "export CORPUS=${CORPUS:-/tmp/ripindex-corpora/cpython-v3.12.0}"  Enter
Type "ripindex index $CORPUS >/dev/null 2>&1"  Enter
Type "clear"  Enter
Show

Type "# 4,579 files, 90 MiB of source. Already indexed."  Enter
Sleep 1500

Type "ripindex search --root $CORPUS PyUnicode_FromString"  Enter
Sleep 3s

Type "clear"  Enter
Type "# Boolean queries, phrases, negation:"  Enter
Sleep 800
Type "ripindex search --root $CORPUS 'asyncio AND subprocess'"  Enter
Sleep 3s

Type "clear"
Type "ripindex search --root $CORPUS '\"reference count\"'"  Enter
Sleep 3s

Type "clear"  Enter
Type "# What is the daemon doing?"  Enter
Sleep 800
Type "ripindex status"  Enter
Sleep 4s
