# docs/img

`demo.gif` is generated, not committed by hand. Produce it with:

```sh
scripts/demo-corpus.sh cpython
CORPUS=/tmp/ripindex-corpora/cpython-v3.12.0 vhs demo/demo.tape
```

That writes `docs/img/demo.gif`, which `README.md` links from the top of the page.
Keep it under a couple of megabytes so the README stays quick to load.
