local data = vim.json.decode(table.concat(vim.fn.readfile(vim.env.MATRIX_MANIFEST), "\n"))
local out = { samples = {}, events = {} }
local client, main, child
local current, sequence, closed = 1, 0, false
local function write_out()
  vim.fn.writefile({ vim.json.encode(out) }, vim.env.MATRIX_OUTPUT)
end
local function done(err)
  if closed then
    return
  end
  closed = true
  out.error = err
  write_out()
  vim.cmd("qa!")
end
local function event(method, r)
  out.events[#out.events + 1] = {
    method = method,
    buildId = r and r.buildId,
    status = r and r.status,
    message = r and r.message,
    stale = r and r.stale,
    logUri = r and r.logUri,
  }
end
local function edit(buf, save)
  sequence = sequence + 1
  local marker = "TEXQUILL_SAVE_PROBE_" .. sequence
  local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
  local index = #lines
  if buf == main then
    for i, line in ipairs(lines) do
      if line:find("\\end{document}", 1, true) then
        index = i - 1
        break
      end
    end
  end
  vim.api.nvim_buf_set_lines(buf, index, index, false, { "\\typeout{" .. marker .. "}" })
  if save then
    vim.api.nvim_buf_call(buf, function()
      vim.cmd("write")
    end)
  end
  return marker
end
local run_case
run_case = function()
  local c = data.cases[current]
  if not c then
    done()
    return
  end
  local start = #out.events
  local record = { name = c.name, ok = false, stale = {} }
  out.samples[#out.samples + 1] = record
  local expect_stale = c.name == "queued_saved_then_unsaved"
  local marker, unsaved
  if c.name == "unchanged_save" then
    vim.api.nvim_buf_call(main, function()
      vim.cmd("write")
    end)
  else
    marker = edit((c.name == "child_changed" or c.name == "child_then_main") and child or main, true)
  end
  if c.name == "two_fast_saves_main" or expect_stale or c.name == "child_then_main" then
    vim.defer_fn(function()
      marker = edit(main, true)
    end, 30)
  end
  if expect_stale then
    vim.defer_fn(function()
      unsaved = edit(main, false)
    end, 60)
  end
  local deadline = vim.uv.hrtime() / 1e6 + 25000
  local function poll()
    for i = start + 1, #out.events do
      local e = out.events[i]
      if e.method == "latex/buildFinished" and e.status == "success" and e.stale == expect_stale then
        local log = e.logUri and table.concat(vim.fn.readfile(vim.uri_to_fname(e.logUri)), "\n") or ""
        if (not marker or log:find(marker, 1, true)) and (not unsaved or not log:find(unsaved, 1, true)) then
          record.ok, record.buildId = true, e.buildId
          if expect_stale then
            record.unsaved_preserved = vim.bo[main].modified and unsaved ~= nil
            record.ok = record.ok and record.unsaved_preserved
          end
          write_out()
          current = current + 1
          vim.defer_fn(run_case, 400)
          return
        end
      end
    end
    if vim.uv.hrtime() / 1e6 > deadline then
      record.error = "save did not compile the latest saved marker"
      done(record.error)
    else
      vim.defer_fn(poll, 50)
    end
  end
  vim.defer_fn(poll, 150)
end
local function setup()
  client = vim.lsp.get_clients({ name = "texquill", bufnr = 0 })[1]
  if not client or not client.initialized then
    vim.defer_fn(setup, 100)
    return
  end
  assert(client.settings.latex.build.onSave, "build.onSave must be enabled")
  main, child = vim.api.nvim_get_current_buf(), vim.api.nvim_get_current_buf()
  if data.sources["Tex/1_Introduction.tex"] then
    child = vim.fn.bufadd(data.root .. "/Tex/1_Introduction.tex")
    vim.fn.bufload(child)
    vim.lsp.buf_attach_client(child, client.id)
  end
  local notify = client.notify
  client.notify = function(...)
    local a = { ... }
    local k = a[1] == client and 2 or 1
    local method, p = a[k], a[k + 1]
    if method == "textDocument/didSave" or method == "workspace/didChangeWatchedFiles" then
      event(method, { message = p.textDocument and p.textDocument.uri })
    end
    return notify(...)
  end
  for _, method in ipairs({ "latex/buildStarted", "latex/buildFinished", "window/logMessage" }) do
    local old = client.handlers[method] or vim.lsp.handlers[method]
    client.handlers[method] = function(err, r, ctx, config)
      event(method, r)
      if old then
        return old(err, r, ctx, config)
      end
    end
  end
  client:request(
    "workspace/executeCommand",
    { command = "latex.build", arguments = { vim.uri_from_bufnr(main) } },
    function(err, r)
      if err then
        done(err.message)
        return
      end
      local deadline = vim.uv.hrtime() / 1e6 + 30000
      local function wait()
        for _, e in ipairs(out.events) do
          if e.method == "latex/buildFinished" and e.buildId == r.buildId then
            if e.status ~= "success" then
              done("baseline build failed: " .. e.status)
            else
              vim.defer_fn(run_case, 400)
            end
            return
          end
        end
        if vim.uv.hrtime() / 1e6 > deadline then
          done("baseline timeout")
        else
          vim.defer_fn(wait, 50)
        end
      end
      wait()
    end
  )
end
vim.defer_fn(setup, 300)
vim.defer_fn(function()
  done("suite timeout")
end, 220000)
