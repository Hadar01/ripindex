--- Minimal ripindex daemon client.
---
--- The daemon speaks newline-delimited JSON over a Unix socket (or a Windows
--- named pipe), which Neovim can open directly with `sockconnect('pipe', ...)`.
--- That is the whole reason this file is short: no RPC library, no shelling
--- out and parsing human-readable output.
local M = {}

local PROTOCOL = 1

--- Mirrors `daemon::paths` in the Rust source.
function M.address()
  if vim.fn.has("win32") == 1 then
    local user = vim.env.USERNAME or "default"
    return [[\\.\pipe\ripindex-]] .. user:gsub("[^%w_-]", "")
  end
  local runtime = vim.env.XDG_RUNTIME_DIR
  if runtime and runtime ~= "" then
    return runtime .. "/ripindex/daemon.sock"
  end
  local state = vim.env.XDG_STATE_HOME
  if not state or state == "" then
    state = (vim.env.HOME or "") .. "/.local/state"
  end
  return state .. "/ripindex/daemon.sock"
end

--- Open a connection and complete the version handshake.
--- Returns `channel, nil` on success or `nil, err`.
--- `on_line` is called with each decoded server message.
function M.connect(on_line)
  local pending = ""
  local ok, chan = pcall(vim.fn.sockconnect, "pipe", M.address(), {
    on_data = function(_, data, _)
      -- `data` is newline-split; the final element is a partial line (or "").
      for i, chunk in ipairs(data) do
        if i < #data then
          local line = pending .. chunk
          pending = ""
          line = line:gsub("\r$", "")
          if line ~= "" then
            local decoded_ok, msg = pcall(vim.json.decode, line)
            if decoded_ok then
              on_line(msg)
            end
          end
        else
          pending = pending .. chunk
        end
      end
    end,
  })

  if not ok or chan == 0 then
    return nil, "no ripindex daemon listening at " .. M.address()
  end
  vim.fn.chansend(chan, vim.json.encode({ type = "hello", protocol = PROTOCOL }) .. "\n")
  return chan, nil
end

--- Start the daemon and wait (briefly) for it to accept connections.
function M.ensure_daemon()
  local exe = vim.fn.exepath("ripindex")
  if exe == "" then
    return false, "ripindex is not on your PATH"
  end
  vim.fn.jobstart({ exe, "daemon" }, { detach = true })
  local deadline = vim.loop.now() + 2000
  while vim.loop.now() < deadline do
    local chan = M.connect(function() end)
    if chan then
      vim.fn.chanclose(chan)
      return true, nil
    end
    vim.wait(50)
  end
  return false, "started the ripindex daemon but it did not come up in time"
end

--- Run one query and return its hits.
--- Blocking, but bounded: server-side queries are microseconds, so the wait
--- only ever covers connect + IPC. Returns `hits, nil` or `nil, err`.
--- @param opts table: root (string), query (string), limit (number|nil), timeout (number|nil)
function M.query(opts)
  local hits, done, err = {}, false, nil
  local id = 1

  -- Declared before the call, not by it: in Lua a `local x = f(function() ... x ... end)`
  -- closure captures the *outer* x, because the new local isn't in scope until
  -- the statement finishes. The callback fires asynchronously, well after the
  -- assignment below, so this reads the connected channel correctly.
  local chan, connect_err
  chan, connect_err = M.connect(function(msg)
    if msg.type == "hello_ok" then
      vim.fn.chansend(
        chan,
        vim.json.encode({
          type = "request",
          id = id,
          method = "query",
          params = {
            roots = { opts.root },
            query = opts.query,
            limit = opts.limit or 100,
            snippet = true,
          },
        }) .. "\n"
      )
    elseif msg.type == "hit" then
      table.insert(hits, msg)
    elseif msg.type == "done" then
      done = true
    elseif msg.type == "error" then
      err = msg.message
      done = true
    end
  end)
  if not chan then
    return nil, connect_err
  end

  vim.wait(opts.timeout or 2000, function()
    return done
  end, 5)
  vim.fn.chanclose(chan)

  if err then
    return nil, err
  end
  if not done then
    return nil, "the ripindex daemon did not answer in time"
  end
  return hits, nil
end

return M
