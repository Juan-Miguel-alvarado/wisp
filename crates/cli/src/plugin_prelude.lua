-- wisp's implementation of yazi's Lua plugin API.
-- The `__ds_*` functions are provided by the Rust side (src/plugin.rs).

-- Url ------------------------------------------------------------------------

local UrlMT = {}

local function url_name(s)
	if s == "/" then
		return nil
	end
	return s:match("([^/]+)/*$")
end

UrlMT.__index = function(u, k)
	local s = rawget(u, "_s")
	if k == "name" then
		return url_name(s)
	elseif k == "parent" then
		if s == "/" or s == "" then
			return nil
		end
		local p = s:gsub("/+$", ""):match("^(.*)/[^/]*$")
		if p == nil then
			return nil
		elseif p == "" then
			p = "/"
		end
		return Url(p)
	elseif k == "stem" then
		local n = url_name(s)
		return n and (n:match("^(.+)%.[^.]+$") or n)
	elseif k == "ext" then
		local n = url_name(s)
		return n and n:match("^.+%.([^.]+)$")
	elseif k == "is_absolute" then
		return s:sub(1, 1) == "/"
	elseif k == "is_regular" then
		return true
	elseif k == "is_search" or k == "is_archive" then
		return false
	elseif k == "has_root" then
		return s:sub(1, 1) == "/"
	elseif k == "base" then
		return nil
	elseif k == "path" or k == "urn" or k == "url" or k == "physical" then
		return u
	elseif k == "key" then
		return url_name(s)
	elseif k == "frag" or k == "domain" or k == "scheme" then
		return nil
	end
	return UrlMT[k]
end
UrlMT.__tostring = function(u) return rawget(u, "_s") end
UrlMT.__eq = function(a, b) return tostring(a) == tostring(b) end
UrlMT.__lt = function(a, b) return tostring(a) < tostring(b) end
UrlMT.__le = function(a, b) return tostring(a) <= tostring(b) end
UrlMT.__concat = function(a, b) return tostring(a) .. tostring(b) end

function UrlMT.join(u, other)
	local o = tostring(other)
	if o:sub(1, 1) == "/" then
		return Url(o)
	end
	local b = rawget(u, "_s")
	if b == "" then
		return Url(o)
	end
	if b:sub(-1) ~= "/" then
		b = b .. "/"
	end
	return Url(b .. o)
end

local function with_slash(s) return s:sub(-1) == "/" and s or s .. "/" end

function UrlMT.starts_with(u, base)
	local a, b = tostring(u), tostring(base)
	return a == b or a:sub(1, #with_slash(b)) == with_slash(b)
end

function UrlMT.ends_with(u, child)
	local a, c = tostring(u), tostring(child)
	return a == c or a:sub(-(#c + 1)) == "/" .. c
end

function UrlMT.into_search(u, domain)
	local v = Url(rawget(u, "_s"))
	rawset(v, "_search", tostring(domain))
	return v
end

function UrlMT.strip_prefix(u, base)
	local a, b = tostring(u), with_slash(tostring(base))
	if a:sub(1, #b) == b then
		return Url(a:sub(#b + 1))
	end
	return nil
end

function Url(s)
	if getmetatable(s) == UrlMT then
		return s
	end
	return setmetatable({ _s = tostring(s) }, UrlMT)
end
Path = { os = Url }

function Err(fmt, ...) return string.format(fmt, ...) end

function Cha(t) return setmetatable(t or {}, { __index = function(_, k) if k == "perm" then return function(self) return __ds_perm(self.mode or 0) end end return nil end }) end

-- Command ------------------------------------------------------------------

local CmdMT = {}
CmdMT.__index = CmdMT

Command = setmetatable({ NULL = 0, PIPED = 1, INHERIT = 2 }, {
	__call = function(_, program)
		return setmetatable({ program = tostring(program), _args = {}, _env = {} }, CmdMT)
	end,
})

function CmdMT:arg(a)
	if type(a) == "table" then
		for _, x in ipairs(a) do
			self._args[#self._args + 1] = tostring(x)
		end
	else
		self._args[#self._args + 1] = tostring(a)
	end
	return self
end
CmdMT.args = CmdMT.arg

function CmdMT:cwd(d)
	self._cwd = tostring(d)
	return self
end

function CmdMT:env(k, v)
	self._env[tostring(k)] = tostring(v)
	return self
end

function CmdMT:stdin(m)
	self._stdin = m
	return self
end

function CmdMT:stdout(m)
	self._stdout = m
	return self
end

function CmdMT:stderr(m)
	self._stderr = m
	return self
end

function CmdMT:memory() return self end

function CmdMT:output() return __ds_cmd_output(self) end

function CmdMT:status() return __ds_cmd_status(self) end

local ChildMT = {}
ChildMT.__index = ChildMT

function CmdMT:spawn()
	local id, err = __ds_cmd_spawn(self)
	if not id then
		return nil, err
	end
	return setmetatable({ _id = id }, ChildMT)
end

function ChildMT:read_line() return __ds_child_read_line(self._id) end
function ChildMT:read_line_with(_) return __ds_child_read_line(self._id) end
function ChildMT:read(_) return __ds_child_read_line(self._id) end
function ChildMT:write_all(s) return __ds_child_write(self._id, s) end
function ChildMT:flush() return true end
function ChildMT:take_stdin() return self end
function ChildMT:take_stdout() return self end
function ChildMT:take_stderr() return self end
function ChildMT:wait() return __ds_child_wait(self._id, false) end
function ChildMT:wait_with_output()
	__ds_child_close_stdin(self._id)
	return __ds_child_wait(self._id, true)
end
function ChildMT:try_wait() return nil end
function ChildMT:start_kill() return __ds_child_kill(self._id) end
function ChildMT:id() return self._id end

-- Files ---------------------------------------------------------------------

local ChaMT = {}
ChaMT.__index = ChaMT
function ChaMT:perm() return __ds_perm(self.mode) end

local FileMT = {}
FileMT.__index = FileMT
__ds_FileMT = FileMT
function FileMT:size() return (not self.cha.is_dir) and self.cha.len or nil end
function FileMT:mime() return nil end
function FileMT:prefix() return nil end
function FileMT:found() return nil end
function FileMT:highlights() return nil end
function FileMT:is_yanked() return __ds_yanked[tostring(self.url)] or 0 end
function FileMT:is_selected() return __ds_selected[tostring(self.url)] == true end
function FileMT:is_marked() return 0 end

local function mk_file(raw)
	local cha = setmetatable({
		is_dir = raw.is_dir,
		is_hidden = raw.is_hidden,
		is_link = raw.is_link,
		is_orphan = raw.is_orphan,
		is_dummy = false,
		is_exec = raw.is_exec,
		is_block = raw.is_block,
		is_char = raw.is_char,
		is_fifo = raw.is_fifo,
		is_sock = raw.is_sock,
		is_sticky = raw.is_sticky,
		len = raw.len,
		mtime = raw.mtime,
		btime = raw.btime,
		atime = raw.atime,
		mode = raw.mode,
		uid = raw.uid,
		gid = raw.gid,
		nlink = raw.nlink,
	}, ChaMT)
	local url = Url(raw.path)
	return setmetatable({
		url = url,
		path = url,
		name = raw.name,
		cha = cha,
		link_to = raw.link_to and Url(raw.link_to) or nil,
		is_hovered = raw.is_hovered == true,
		in_current = true,
		_flags = raw.flags or 0,
		_mime = raw.mime or "",
	}, FileMT)
end
__ds_mk_file = mk_file

function File(t)
	local url = Url(t.url)
	return setmetatable({ url = url, path = url, name = url.name, cha = t.cha or Cha {}, link_to = t.link_to }, FileMT)
end

__ds_yanked, __ds_selected = {}, {}

local function mk_folder(raw)
	if not raw then
		return nil
	end
	local f = {
		cwd = Url(raw.cwd),
		cursor = raw.cursor,
		offset = raw.offset,
		files = {},
		window = {},
		stage = function() return true, nil end,
	}
	for i, x in ipairs(raw.files) do
		f.files[i] = mk_file(x)
	end
	for i, x in ipairs(raw.window) do
		f.window[i] = mk_file(x)
	end
	f.hovered = raw.hovered and mk_file(raw.hovered) or nil
	return f
end

function __ds_wrap_cx(raw)
	__ds_yanked, __ds_selected = {}, {}
	for _, p in ipairs(raw.yanked) do
		__ds_yanked[p] = raw.cut and 2 or 1
	end
	local tabs = { idx = raw.cur }
	for i, t in ipairs(raw.tabs) do
		local selected = {}
		for j, p in ipairs(t.selected) do
			selected[j] = Url(p)
			if i == raw.cur then
				__ds_selected[p] = true
			end
		end
		local mode = { is_select = t.mode == "select", is_unset = t.mode == "unset" }
		mode.is_visual = mode.is_select or mode.is_unset
		if mode.is_visual then
			mode.visual = { start = t.visual_start or 0, wraps = 0 }
		end
		setmetatable(mode, { __tostring = function() return t.mode end })
		local pv = { skip = t.preview.skip, folder = mk_folder(t.preview.folder) }
		local tab = {
			id = i,
			idx = i,
			name = t.name,
			mode = mode,
			pref = t.pref,
			current = mk_folder(t.current),
			parent = mk_folder(t.parent),
			preview = pv,
			selected = selected,
			finder = nil,
		}
		function tab:history(url) return nil end
		tabs[i] = tab
	end
	local yanked = { is_cut = raw.cut }
	for i, p in ipairs(raw.yanked) do
		yanked[i] = Url(p)
	end
	cx = {
		active = tabs[raw.cur],
		tabs = tabs,
		yanked = yanked,
		tasks = { progress = { total = 0, succ = 0, fail = 0, found = 0, processed = 0 }, snaps = {}, cursor = 0 },
		layer = "mgr",
	}
end

-- fs ------------------------------------------------------------------------

fs = {}

function fs.cha(url, follow)
	local raw, err = __ds_fs_cha(tostring(url), follow)
	if not raw then
		return nil, err
	end
	return mk_file(raw).cha
end

function fs.read_dir(url, opts)
	local list, err = __ds_fs_read_dir(tostring(url), opts and opts.limit or 0)
	if not list then
		return nil, err
	end
	for i, f in ipairs(list) do
		list[i] = mk_file(f)
	end
	return list
end

function fs.write(url, data) return __ds_fs_write(tostring(url), data) end
function fs.read(url) return __ds_fs_read(tostring(url)) end
function fs.remove(kind, url) return __ds_fs_remove(kind, tostring(url)) end
function fs.create(kind, url) return __ds_fs_create(kind, tostring(url)) end
function fs.unique_name(url) return Url(__ds_fs_unique(tostring(url))) end
fs.unique = fs.unique_name
function fs.file(url)
	local raw, err = __ds_fs_cha(tostring(url), false)
	if not raw then
		return nil, err
	end
	return mk_file(raw)
end
function fs.rename(a, b) return __ds_fs_rename(tostring(a), tostring(b)) end
function fs.copy(a, b) return __ds_fs_copy(tostring(a), tostring(b)) end
function fs.partitions()
	local list, err = __ds_partitions()
	if not list then
		return nil, err
	end
	for _, p in ipairs(list) do
		if p.dist then
			p.dist = Url(p.dist)
		end
	end
	return list
end
function fs.op(kind, t)
	t = t or {}
	t._op = kind
	return t
end
function fs.cwd() return Url(__ds_cwd()) end
function fs.expand_url(url) return Url(url) end
function fs.access() return { read = function(self) return self end, open = function() return nil, "unsupported" end } end

-- ya ------------------------------------------------------------------------

ya = {}

function ya.emit(cmd, args)
	args = args or {}
	-- Inside a previewer, `peek` re-requests with a corrected offset.
	if cmd == "peek" and __ds_out then
		__ds_out.peek = tonumber(args[1])
		return
	end
	local _, name = tostring(cmd):match("^([%w_]-):?([%w_]+)$")
	name = name or cmd
	-- A plugin-built virtual folder (e.g. vcs-files): `cd` into a search URL,
	-- then feed it files with `update_files`.
	if name == "cd" and args[1] and type(args[1]) == "table" and rawget(args[1], "_search") then
		return __ds_emit("__search_begin", { rawget(args[1], "_search"), tostring(args[1]) })
	end
	if name == "update_files" and type(args.op) == "table" then
		local op = args.op
		if op._op == "part" then
			local paths = {}
			for _, f in ipairs(op.files or {}) do
				paths[#paths + 1] = tostring(f.url)
			end
			if #paths > 0 then
				return __ds_emit("__search_files", paths)
			end
			return
		elseif op._op == "done" then
			return __ds_emit("__search_done", {})
		else
			return __ds_emit("update_files", { op.url and tostring(op.url) or nil })
		end
	end
	__ds_emit(cmd, args)
end
ya.mgr_emit = ya.emit
ya.manager_emit = ya.emit
ya.app_emit = ya.emit

function ya.notify(t) __ds_notify(t) end

local RecvMT = { __index = {
	recv = function(self)
		local open = not self._opened
		self._opened = true
		return coroutine.yield { op = "input_recv", open = open, title = self._title, value = self._value, obscure = self._obscure }
	end,
} }

function ya.input(opts)
	opts = opts or {}
	local title, value = tostring(opts.title or ""), tostring(opts.value or "")
	if opts.realtime then
		return setmetatable({ _title = title, _value = value, _obscure = opts.obscure }, RecvMT)
	end
	return coroutine.yield { op = "input", title = title, value = value, obscure = opts.obscure }
end

function ya.confirm(opts)
	opts = opts or {}
	local body = opts.body or opts.content or ""
	if type(body) ~= "string" then
		body = ""
	end
	return coroutine.yield { op = "confirm", title = tostring(opts.title or ""), body = body }
end

function ya.which(opts) return coroutine.yield { op = "which", cands = opts.cands or {}, silent = opts.silent == true } end

local PermitMT = { __index = { drop = function(self)
	if not self._dropped then
		self._dropped = true
		coroutine.yield { op = "show" }
	end
end } }
PermitMT.__close = function(self) self:drop() end

function ya.hide()
	coroutine.yield { op = "hide" }
	return setmetatable({}, PermitMT)
end

__ds_current = nil

-- `ya.sync` blocks run with the plugin's state; everything already runs on
-- the UI thread here, so they are plain calls.
function ya.sync(f)
	local owner = __ds_current
	return function(...)
		local m = owner and package.loaded[owner] or {}
		return f(m, ...)
	end
end
function ya.async(f, ...) return f(...) end
function ya.co(f) return coroutine.wrap(f) end
function ya.sleep(s) __ds_sleep(s) end
function ya.time() return __ds_time() end
function ya.target_os() return "linux" end
function ya.target_family() return "unix" end
function ya.hash(s) return __ds_hash(tostring(s)) end
function ya.quote(s) return __ds_quote(tostring(s)) end
function ya.truncate(s, opts)
	local max = opts and opts.max or #s
	if #s <= max then
		return s
	end
	if opts and opts.rtl then
		return "…" .. s:sub(#s - max + 2)
	end
	return s:sub(1, math.max(0, max - 1)) .. "…"
end
function ya.clipboard(s) return __ds_clipboard(s) end
function ya.readable_size(n) return __ds_readable_size(n) end
function ya.readable_path(p) return __ds_readable_path(tostring(p)) end
function ya.user_name(uid) return __ds_user_name(uid) end
function ya.group_name(gid) return __ds_group_name(gid) end
function ya.uid() return (__ds_ids()) end
function ya.gid() local _, g = __ds_ids() return g end
function ya.host_name() return __ds_host_name() end
local function log(level, ...)
	local parts = {}
	for i = 1, select("#", ...) do
		parts[#parts + 1] = tostring((select(i, ...)))
	end
	__ds_log(level, table.concat(parts, " "))
end
function ya.err(...) log("error", ...) end
function ya.dbg(...) log("debug", ...) end
function ya.list_merge(a, b)
	for _, v in ipairs(b) do
		a[#a + 1] = v
	end
	return a
end
function ya.dict_merge(a, b)
	for k, v in pairs(b) do
		a[k] = v
	end
	return a
end
function ya.clamp(min, x, max) return math.max(min, math.min(max, x)) end
function ya.round(x) return math.floor(x + 0.5) end
local ids = 0
function ya.id(_)
	ids = ids + 1
	return ids
end
-- Previewer / spotter output is captured and rendered natively.
__ds_out = nil
function ya.preview_widget(_, w)
	if __ds_out and w ~= nil then
		__ds_out.widgets = w
	end
end
ya.preview_widgets = ya.preview_widget
function ya.preview_code(_)
	if __ds_out then
		__ds_out.native = "code"
	end
	return true
end
function ya.image_show(url, area)
	if __ds_out then
		__ds_out.image = tostring(url)
	else
		-- Outside a previewer (e.g. `zoom`'s entry): show it in the preview.
		__ds_emit("__show_image", { tostring(url) })
	end
	return area
end
function ya.image_precache(src, dst) return fs.copy(src, dst) end
function ya.image_info(url)
	local w, h, format = __ds_image_info(tostring(url))
	if not w then
		return nil
	end
	return { w = w, h = h, format = format, color = "RGB8" }
end
function ya.file_cache(job) return Url(__ds_cache_path(tostring(job.file.url), job.skip or 0)) end
function ya.spot_table(_, t)
	if __ds_out then
		__ds_out.table = t
	end
end
function ya.spot_widgets(_, w)
	if __ds_out then
		__ds_out.widgets = w
	end
end
function ya.render() __ds_render() end
function ya.exec() return nil end
function ya.json_decode(s) return __ds_json_decode(s) end
function ya.json_encode(v) return __ds_json_encode(v) end
-- Concurrency: `ya.join`/`ya.select` run their functions as coroutines,
-- switching whenever one waits on an empty channel. Requests for the host
-- (ya.which, ya.input, …) are passed up to the app and the answer handed back
-- to the coroutine that asked.
local join_depth = 0

local function schedule(fns, first_only)
	local tasks = {}
	for i, f in ipairs(fns) do
		tasks[i] = { co = coroutine.create(f) }
	end
	local results = {}
	join_depth = join_depth + 1
	while true do
		local progressed, alive = false, false
		for i, t in ipairs(tasks) do
			if not t.done then
				alive = true
				if not t.wait or #t.wait > 0 then
					t.wait = nil
					local args = t.args or {}
					t.args = nil
					local r = table.pack(coroutine.resume(t.co, table.unpack(args, 1, args.n)))
					if not r[1] then
						join_depth = join_depth - 1
						error(r[2], 0)
					end
					progressed = true
					if coroutine.status(t.co) == "dead" then
						t.done, results[i] = true, table.pack(table.unpack(r, 2, r.n))
						if first_only then
							join_depth = join_depth - 1
							return table.unpack(results[i], 1, results[i].n)
						end
					elseif type(r[2]) == "table" and r[2].__chan_wait then
						t.wait = r[2].q
					else
						t.args = table.pack(coroutine.yield(r[2]))
					end
				end
			end
		end
		if not alive or not progressed then
			break
		end
	end
	join_depth = join_depth - 1
	local out = {}
	for i = 1, #tasks do
		out[i] = results[i] and results[i][1]
	end
	return table.unpack(out, 1, #tasks)
end

function ya.join(...) return schedule({ ... }, false) end
function ya.select(...) return schedule({ ... }, true) end

function ya.chan(_, _)
	local q = {}
	local tx = {
		send = function(_, v)
			q[#q + 1] = v
			return true
		end,
	}
	local rx = {
		recv = function()
			while #q == 0 and join_depth > 0 do
				coroutine.yield { __chan_wait = true, q = q }
			end
			return table.remove(q, 1)
		end,
	}
	return tx, rx
end
function ya.clone(v) return v end
function ya.task()
	local t
	t = setmetatable({}, { __index = function() return function() return t end end })
	return t
end

-- ps ------------------------------------------------------------------------

ps = { _subs = {} }
function ps.sub(kind, f)
	ps._subs[kind] = ps._subs[kind] or {}
	table.insert(ps._subs[kind], { f = f, owner = __ds_current })
end
ps.sub_remote = ps.sub
function ps.unsub(kind) ps._subs[kind] = nil end
ps.unsub_remote = ps.unsub
function __ds_fire(kind, body)
	for _, s in ipairs(ps._subs[kind] or {}) do
		local ok, e = pcall(s.f, body)
		if not ok then
			ya.err(e)
		end
	end
end
function __ds_has_sub(kind) return ps._subs[kind] ~= nil and #ps._subs[kind] > 0 end
function ps.pub(kind, body) __ds_fire(kind, body) end
function ps.pub_to(_, kind, body) __ds_fire(kind, body) end
ps.pub_static = ps.pub

rt = __ds_rt
do
	local r = rt.mgr.ratio
	r[1], r[2], r[3] = r.parent, r.current, r.preview
end
-- `rt.plugin.fetchers` & co.: the configured rule lists, editable in Lua.
local RulesMT = {}
RulesMT.__index = RulesMT
function RulesMT:match() return self._list end
function RulesMT:remove(o)
	for i, r in ipairs(self._list) do
		if (o.id and r.id == o.id) or (o.name and r.run == o.name) then
			table.remove(self._list, i)
			return true
		end
	end
	return false
end
function RulesMT:insert(i, r)
	table.insert(self._list, math.min(i, #self._list + 1), r)
	return true
end
rt.plugin = {}
for _, k in ipairs { "fetchers", "spotters", "preloaders", "previewers" } do
	local list = {}
	for i, r in ipairs((__ds_rules or {})[k] or {}) do
		list[i] = { id = i, run = r.run, url = r.url, mime = r.mime, name = r.run }
	end
	rt.plugin[k] = setmetatable({ _list = list }, RulesMT)
end
rt.tasks = {}
rt.term = {
	light = function() return false end,
	cell_size = function() return __ds_cell_size() end,
}

-- yazi's built-in plugins, as modules Lua plugins can call into. Their work
-- is done natively: they only tell the Rust side what to render.

local function native(name)
	local m = { _id = name }
	function m:peek(job)
		if __ds_out then
			__ds_out.native = name
		end
	end
	function m:seek() end
	function m:preload() return true end
	function m:fetch() return true end
	function m:spot(job)
		if __ds_out then
			__ds_out.native = "spot"
		end
	end
	function m:spot_base(job)
		local f = job.file
		local function date(t) return t and t > 0 and os.date("%Y-%m-%d %H:%M:%S", math.floor(t)) or "-" end
		return {
			ui.Row({ "Base" }):style(ui.Style():fg("green")),
			ui.Row { "  Created:", date(f.cha.btime) },
			ui.Row { "  Modified:", date(f.cha.mtime) },
			ui.Row { "  Mimetype:", job.mime or "-" },
			ui.Row {},
		}
	end
	function m:msg(job, s) ya.preview_widget(job, ui.Text(ui.Line(s):reverse())) end
	return m
end
for _, n in ipairs { "code", "file", "image", "folder", "empty", "json", "pdf", "video", "archive", "font", "magick", "svg", "noop", "null", "vfs", "mime", "mime.local", "mime.dir", "dds", "extract", "session" } do
	package.loaded[n] = native(n)
end

local function file_job(raw, extra)
	local job = extra or {}
	job.file = __ds_mk_file(raw)
	job.args = job.args or {}
	return job
end

function __ds_peek(name, raw, mime, skip, w, h, args)
	__ds_out = {}
	local job = file_job(raw, { mime = mime, skip = skip, area = ui.Rect { x = 0, y = 0, w = w, h = h }, args = args })
	job.file.is_hovered = true
	if __ds_preview_thread then
		-- Previewers run on their own thread: their `cx` is the file being
		-- previewed, which is what is hovered.
		local f = job.file
		cx = {
			active = {
				current = { cwd = f.url.parent, hovered = f, files = { f }, window = { f }, cursor = 0, offset = 0 },
				preview = { skip = skip },
				selected = {},
				mode = { is_select = false, is_unset = false, is_visual = false },
				pref = {},
			},
			tabs = {},
			yanked = {},
		}
		cx.tabs[1], cx.tabs.idx = cx.active, 1
	end
	local prev = __ds_current
	__ds_current = name
	local ok, err = pcall(function() require(name):peek(job) end)
	__ds_current = prev
	local out = __ds_out
	__ds_out = nil
	if not ok then
		out.error = tostring(err)
	end
	return out
end

function __ds_spot(name, raw, mime)
	__ds_out = {}
	local job = file_job(raw, { mime = mime, skip = 0 })
	local prev = __ds_current
	__ds_current = name
	local ok, err = pcall(function() require(name):spot(job) end)
	__ds_current = prev
	local out = __ds_out
	__ds_out = nil
	if not ok then
		out.error = tostring(err)
	end
	return out
end

function __ds_fetch(name, raws, args)
	local files = {}
	for i, r in ipairs(raws) do
		files[i] = __ds_mk_file(r)
	end
	local prev = __ds_current
	__ds_current = name
	local ok, err = pcall(function() require(name):fetch { files = files, args = args or {} } end)
	__ds_current = prev
	if not ok then
		return tostring(err)
	end
	return nil
end

-- require / entry -----------------------------------------------------------

local orig_require = require
function require(name)
	local m = package.loaded[name]
	if m ~= nil then
		return m
	end
	local src, path
	if name:sub(1, 1) == "." and __ds_current then
		-- A file inside the current plugin: `require(".cross")`.
		local base = __ds_current:match("^[^.]+")
		local key = base .. name
		if package.loaded[key] ~= nil then
			return package.loaded[key]
		end
		src, path = __ds_find_plugin_file(base, name:sub(2))
		if not src then
			error(string.format("module `%s` not found in plugin `%s`", name, base))
		end
		local chunk, err = load(src, "@" .. path, "t", _ENV)
		if not chunk then
			error(err)
		end
		local m2 = chunk() or {}
		package.loaded[key] = m2
		return m2
	end
	src, path = __ds_find_plugin(name)
	if not src then
		return orig_require(name)
	end
	local chunk, err = load(src, "@" .. path, "t", _ENV)
	if not chunk then
		error(err)
	end
	local prev = __ds_current
	__ds_current = name
	m = chunk() or {}
	__ds_current = prev
	if type(m) == "table" then
		m._id = name
	end
	package.loaded[name] = m
	return m
end

function __ds_run(name, job)
	local prev = __ds_current
	__ds_current = name
	local m = require(name)
	if type(m) ~= "table" or type(m.entry) ~= "function" then
		error(string.format("plugin `%s` has no `entry` function", name))
	end
	m:entry(job)
	__ds_current = prev
end
