# ripindex.nvim

A [Telescope](https://github.com/nvim-telescope/telescope.nvim) extension that queries the
ripindex daemon directly over its socket.

## Why it's this short

The daemon protocol is newline-delimited JSON over a Unix socket (or a Windows named pipe),
and Neovim opens those natively with `vim.fn.sockconnect('pipe', ...)`. So the whole client
is one small Lua file: no RPC library, and no shelling out to parse human-readable CLI
output. That was the point of designing the protocol rather than only shipping a CLI.

## Install

**lazy.nvim**

```lua
{
  "hadar01/ripindex",
  -- the Lua lives in a subdirectory of the main repo
  config = function()
    vim.opt.rtp:append(vim.fn.stdpath("data") .. "/lazy/ripindex/contrib/nvim")
    require("telescope").load_extension("ripindex")
  end,
  dependencies = { "nvim-telescope/telescope.nvim" },
}
```

**Manual** — copy `contrib/nvim/lua/` into any directory on your `runtimepath`, then:

```lua
require("telescope").load_extension("ripindex")
```

The `ripindex` binary must be on your `PATH`. The extension starts the daemon on first use
if it isn't already running, exactly as the CLI does.

## Use

```vim
:Telescope ripindex search
```

Type to search; results update per keystroke. Full query syntax works, so `foo AND -test`
and `"exact phrase"` do what you'd expect.

```lua
-- Search a specific root rather than the cwd
require("telescope").extensions.ripindex.search({ root = "~/code/bigrepo" })

-- Suggested mapping
vim.keymap.set("n", "<leader>fi", function()
  require("telescope").extensions.ripindex.search()
end, { desc = "ripindex search" })
```

## Notes

- Prompts shorter than two characters return nothing — a one-character query matches a
  large fraction of any corpus, and rendering that is slower than the search itself.
- Queries are bounded-blocking (`vim.wait`, 2s cap). Server-side queries are microseconds,
  so in practice the wait only covers the IPC round trip.
- **Untested.** This plugin was written without Neovim available on the development
  machine, so it has not been run. It is short and readable for exactly that reason —
  please read it before trusting it, and report anything broken.
