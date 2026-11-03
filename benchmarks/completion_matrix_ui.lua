local data = vim.json.decode(table.concat(vim.fn.readfile(vim.env.MATRIX_MANIFEST), "\n"))
local uv = vim.uv
local results, requests, events = {}, {}, {}
local active, client, ui, original, closed, run_case
local buffers = {}
local function now()
  return uv.hrtime() / 1e6
end
local function has(values, value)
  for _, v in ipairs(values) do
    if v == value then
      return true
    end
  end
  return false
end
local function finish(err)
  if closed then
    return
  end
  closed = true
  if ui and original then
    ui.render = original
  end
  vim.fn.writefile(
    { vim.json.encode({ samples = results, error = err, events = events, mode = data.mode }) },
    vim.env.MATRIX_OUTPUT
  )
  vim.cmd("qa!")
end
vim.defer_fn(function()
  finish("suite timeout")
end, 240000)
vim.api.nvim_create_autocmd("LspRequest", {
  callback = function(ev)
    local d = ev.data
    if client and d.client_id == client.id and d.request.method == "textDocument/completion" then
      if d.request.type == "pending" then
        requests[d.request_id] = { t = now(), case = active and active.case.name }
      end
      if d.request.type == "complete" and requests[d.request_id] then
        local r = requests[d.request_id]
        events[#events + 1] = { case = r.case, ms = now() - r.t }
        requests[d.request_id] = nil
      end
    end
  end,
})
local function next_case()
  if vim.snippet and vim.snippet.active() then
    vim.snippet.stop()
  end
  require("completion_tools").hide()
  vim.api.nvim_input("<Esc>")
  local index = active and active.index + 1 or 1
  active = nil
  vim.defer_fn(function()
    run_case(index)
  end, 100)
end
local function case_done(record)
  if not active or active.done then
    return
  end
  if not record.ok then
    record.trace = active.trace
    local mapping = vim.fn.maparg("\\", "i", false, true)
    record.backslash_mapping = { desc = mapping.desc, rhs = mapping.rhs }
  end
  active.done = true
  results[#results + 1] = record
  local progress = { samples = results, events = events, mode = data.mode, in_progress = true }
  vim.fn.writefile({ vim.json.encode(progress) }, vim.env.MATRIX_OUTPUT)
  if data.mode == "build" or data.mode == "save" then
    active.after_finish = function()
      vim.defer_fn(next_case, 400)
    end
    if active.build_finished then
      active.after_finish()
      active.after_finish = nil
    end
  else
    vim.defer_fn(next_case, 150)
  end
end
local function start_typing()
  local a = active
  local c = a.case
  local lines = vim.split(a.source, "\n", { plain = true })
  if lines[#lines] == "" then
    table.remove(lines)
  end
  local marker = c.preamble and "\\begin{document}"
    or (
      c.document == "main.tex" and "\\maketitle"
      or (c.name == "abstract_textbf" and "\\begin{abstract}" or "\\section{Introduction}")
    )
  local index
  for i, line in ipairs(lines) do
    if line:find(marker, 1, true) then
      index = c.preamble and i - 1 or i
      break
    end
  end
  assert(index, "anchor missing: " .. c.name)
  local head = vim.split(c.head, "\n", { plain = true })
  local tail = (c.tail == "}") and "" or c.tail
  local extra = vim.split(c.head .. tail, "\n", { plain = true })
  if data.mode == "rapid" then
    table.insert(extra, 1, "")
  end
  if #extra == 0 then
    extra = { "" }
  end
  vim.api.nvim_buf_set_lines(a.buf, index, index, false, extra)
  local row = index + #head + (data.mode == "rapid" and 1 or 0)
  local col = #head[#head]
  local input_row = data.mode == "rapid" and index + 1 or row
  local input_col = data.mode == "rapid" and 0 or col
  vim.api.nvim_win_set_cursor(0, { input_row, input_col })
  vim.api.nvim_input("i")
  -- Restore the byte position after InsertEnter: normal-mode conceal may move
  -- a cursor placed on the backslash of a closing math delimiter.
  vim.defer_fn(function()
    if active == a and vim.api.nvim_get_mode().mode:sub(1, 1) == "i" then
      vim.api.nvim_win_set_cursor(0, { input_row, input_col })
    end
  end, 5)
  a.trace = {}
  local text = c.prefix
  if data.mode == "rapid" then
    text = "typing " .. string.rep("x", 20) .. "\n" .. text
  end
  a.typing = true
  for i = 1, #text do
    local ch = text:sub(i, i)
    vim.defer_fn(function()
      if active ~= a or a.done then
        return
      end
      a.trace[#a.trace + 1] = {
        key = ch,
        cursor = vim.api.nvim_win_get_cursor(0),
        line = vim.api.nvim_get_current_line(),
        mode = vim.api.nvim_get_mode().mode,
      }
      if i == #text then
        a.last = now()
      end
      if ch == "\n" then
        require("completion_tools").hide()
        vim.api.nvim_win_set_cursor(0, { row, col })
      else
        vim.api.nvim_input(ch)
      end
    end, i * 20)
  end
  vim.defer_fn(function()
    if active == a and not a.done then
      case_done({ name = c.name, ok = false, error = "candidate timeout", stale = a.stale })
    end
  end, 12000)
end
run_case = function(index)
  if closed then
    return
  end
  local c = data.cases[index]
  if not c then
    finish()
    return
  end
  require("completion_tools").hide()
  for name, source in pairs(data.sources) do
    local buf = buffers[name]
    local lines = vim.split(source, "\n", { plain = true })
    if lines[#lines] == "" then
      table.remove(lines)
    end
    if data.churn_buffers or not vim.deep_equal(vim.api.nvim_buf_get_lines(buf, 0, -1, false), lines) then
      vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
    end
  end
  local buf = buffers[c.document]
  vim.api.nvim_set_current_buf(buf)
  active = {
    case = c,
    index = index,
    buf = buf,
    source = data.sources[c.document],
    stale = {},
    started = now(),
  }
  local function attached()
    if not active or active.index ~= index then
      return
    end
    local available = vim.lsp.get_clients({ name = "texquill", bufnr = buf })[1]
    if not available or not available.initialized then
      vim.defer_fn(attached, 100)
      return
    end
    client = available
    if vim.env.MATRIX_NO_WATCH == "1" then
      require("vim.lsp._watchfiles").cancel(client.id)
    end
    vim.defer_fn(function()
      local a = active
      if not a or a.index ~= index then
        return
      end
      if data.mode == "build" then
        client:request(
          "workspace/executeCommand",
          { command = "latex.build", arguments = { vim.uri_from_fname(data.root .. "/main.tex") } },
          function(err, result)
            a.build_reply = { error = err, result = result }
          end,
          buffers["main.tex"]
        )
      elseif data.mode == "save" then
        assert(client.settings.latex.build.onSave)
        vim.api.nvim_buf_call(buffers["main.tex"], function()
          vim.cmd("write")
        end)
      end
      if c.arity then
        a.source = a.source:gsub("\\codexprobe}%[2%]{#1#2}", "\\codexprobe}[1]{#1}")
        local lines = vim.split(a.source, "\n", { plain = true })
        if lines[#lines] == "" then
          table.remove(lines)
        end
        vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
      end
      vim.defer_fn(start_typing, (c.pause or 0) * 1000 + 20)
    end, 600)
  end
  attached()
end
local function setup()
  client = vim.lsp.get_clients({ name = "texquill", bufnr = 0 })[1]
  if not client or not client.initialized then
    vim.defer_fn(setup, 100)
    return
  end
  local notify_user = vim.notify
  vim.notify = function(message, level, opts)
    events[#events + 1] = { method = "editorMessage", message = tostring(message), case = active and active.case.name }
    return notify_user(message, level, opts)
  end
  local request = client.request
  client.request = function(...)
    local args = { ... }
    local shift = args[1] == client and 1 or 0
    local method, callback = args[1 + shift], args[3 + shift]
    if method == "textDocument/completion" and callback then
      local started, name = now(), active and active.case.name
      args[3 + shift] = function(err, result, ...)
        events[#events + 1] = {
          method = "completionResponse",
          case = name,
          ms = now() - started,
          error = err,
          count = result and result ~= vim.NIL and #(result.items or result) or 0,
          position = args[2 + shift].position,
        }
        return callback(err, result, ...)
      end
    end
    return request(unpack(args))
  end
  local notify = client.notify
  client.notify = function(...)
    local self, method, params = ...
    if self ~= client then
      method, params = self, method
    end
    if method == "workspace/didChangeWatchedFiles" or method == "textDocument/didSave" then
      events[#events + 1] = {
        method = method,
        case = active and active.case.name,
        since_start = active and now() - active.started,
        params = params,
      }
    end
    return notify(...)
  end
  if vim.env.MATRIX_NO_CHKTEX == "1" then
    local settings = vim.deepcopy(client.settings)
    settings.latex.diagnostics = settings.latex.diagnostics or {}
    settings.latex.diagnostics.chktex = settings.latex.diagnostics.chktex or {}
    settings.latex.diagnostics.chktex.enabled = false
    client:notify("workspace/didChangeConfiguration", { settings = settings })
    client.settings = settings
  end
  require("lazy").load({ plugins = { "completion-tools.nvim" } })
  if vim.env.MATRIX_RAW_BACKSLASH == "1" then
    pcall(vim.keymap.del, "i", "\\")
  end
  ui = require("completion_tools.ui")
  original = ui.render
  ui.render = function(items, selected, ctx, config)
    local t = now()
    local result = original(items, selected, ctx, config)
    local a = active
    if not a or not a.last or a.busy or a.done or ctx.before:sub(-#a.case.prefix) ~= a.case.prefix then
      return result
    end
    if a.case.tail == "\\)" and ctx.line:sub(-2) ~= "\\)" then
      case_done({
        name = a.case.name,
        ok = false,
        error = "input mapping changed closing math delimiter",
        line = ctx.line,
        owner = "pairs.nvim",
      })
      return result
    end
    for _, entry in ipairs(items) do
      local item = entry.resolved or entry.item
      if entry.source_id == "lsp" and item.label == a.case.label then
        local edit = item.textEdit
        if not edit or not has(a.case.edits, edit.newText) then
          a.stale[#a.stale + 1] = { ms = t - a.last, item = item }
          break
        end
        local range = require("completion_tools.edits").range(entry, ctx)
        local cursor = vim.api.nvim_win_get_cursor(0)
        if range and range["end"].line == cursor[1] - 1 and range["end"].character == cursor[2] then
          a.busy = true
          local record = {
            name = a.case.name,
            menu_ms = t - a.last,
            range = range,
            edit = edit,
            stale = a.stale,
          }
          vim.schedule(function()
            local completion = require("completion_tools")
            local current = completion.get_items()
            local chosen = completion.get_selected_item()
            local target, selection
            for i, v in ipairs(current) do
              if v == chosen then
                selection = i
              end
              if v.label == a.case.label and v.textEdit and has(a.case.edits, v.textEdit.newText) then
                target = i
              end
            end
            if not target then
              a.busy = false
              return
            end
            if target ~= (selection or 0) then
              completion.select_next({ count = target - (selection or 0) })
            end
            record.accepted = completion.accept()
            vim.defer_fn(function()
              record.line = vim.api.nvim_get_current_line()
              record.ok = record.accepted and has(a.case.lines, record.line)
              case_done(record)
            end, 50)
          end)
          break
        end
      end
    end
    return result
  end
  local old = client.handlers["latex/buildFinished"] or vim.lsp.handlers["latex/buildFinished"]
  local priming = true
  client.handlers["latex/buildFinished"] = function(err, result, ctx, config)
    if old then
      old(err, result, ctx, config)
    end
    if priming then
      priming = false
      vim.defer_fn(function()
        run_case(1)
      end, 1000)
    elseif active then
      active.build_finished = true
      if active.after_finish then
        active.after_finish()
        active.after_finish = nil
      end
    end
  end
  if data.mode == "build" or data.mode == "save" then
    client:request(
      "workspace/executeCommand",
      { command = "latex.build", arguments = { vim.uri_from_fname(data.root .. "/main.tex") } },
      function(err)
        if err then
          finish(vim.inspect(err))
        end
      end,
      0
    )
  else
    priming = false
    vim.defer_fn(function()
      run_case(1)
    end, 4000)
  end
end
for name in pairs(data.sources) do
  local b = vim.fn.bufadd(data.root .. "/" .. name)
  vim.fn.bufload(b)
  buffers[name] = b
end
setup()
