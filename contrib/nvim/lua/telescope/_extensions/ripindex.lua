--- Telescope extension for ripindex.
---
---   :Telescope ripindex search      prompt-as-you-type over the indexed root
---
--- Results come from the daemon over its socket, so each keystroke costs an
--- IPC round trip and a microsecond-scale query rather than a tree scan.
local ok, pickers = pcall(require, "telescope.pickers")
if not ok then
  error("telescope.nvim is required for the ripindex extension")
end
local finders = require("telescope.finders")
local conf = require("telescope.config").values
local previewers = require("telescope.previewers")
local client = require("ripindex.client")

local function notify(msg, level)
  vim.notify("[ripindex] " .. msg, level or vim.log.levels.WARN)
end

--- One hit -> one Telescope entry. `line_no` and `snippet` come from the
--- daemon, so the picker needs no extra file reads to show context.
local function make_entry(hit)
  local path = hit.path
  local display_path = vim.fn.fnamemodify(path, ":.")
  local lnum = hit.line_no or 1
  local snippet = (hit.snippet or ""):gsub("%s+", " ")
  return {
    value = path,
    display = string.format("%s:%d  %s", display_path, lnum, snippet),
    ordinal = display_path .. " " .. snippet,
    path = path,
    lnum = lnum,
    col = 1,
  }
end

local function search(opts)
  opts = opts or {}
  local root = opts.root or opts.cwd or vim.loop.cwd()
  -- normalize first so only "/" can be trailing: it turns Windows
  -- backslashes into forward slashes, which keeps this pattern free of
  -- backslash escaping (and Lua rejects "\]" as an escape anyway).
  root = vim.fs.normalize(vim.fn.fnamemodify(root, ":p")):gsub("/$", "")

  -- Autostart on first use, the same way the CLI does, so the plugin works
  -- without the user having thought about the daemon at all.
  local probe = client.connect(function() end)
  if probe then
    vim.fn.chanclose(probe)
  else
    local started, err = client.ensure_daemon()
    if not started then
      notify(err or "could not start the daemon", vim.log.levels.ERROR)
      return
    end
  end

  pickers
    .new(opts, {
      prompt_title = "ripindex  (" .. vim.fn.fnamemodify(root, ":~") .. ")",
      finder = finders.new_dynamic({
        entry_maker = make_entry,
        fn = function(prompt)
          -- Single-character prompts match a large share of any corpus; the
          -- round trip is cheap but rendering thousands of entries is not.
          if not prompt or #prompt < 2 then
            return {}
          end
          local hits, err = client.query({
            root = root,
            query = prompt,
            limit = opts.limit or 200,
          })
          if not hits then
            -- A malformed in-progress query (a half-typed quote, say) is
            -- expected while typing: show nothing rather than an error popup.
            if err and not err:match("^invalid query") then
              notify(err)
            end
            return {}
          end
          return hits
        end,
      }),
      previewer = previewers.vim_buffer_vimgrep.new(opts),
      sorter = conf.generic_sorter(opts),
    })
    :find()
end

return require("telescope").register_extension({
  exports = {
    ripindex = search,
    search = search,
  },
})
