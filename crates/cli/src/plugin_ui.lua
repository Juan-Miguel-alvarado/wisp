-- wisp's implementation of yazi's `ui` module and UI components.
-- Widgets are plain tables tagged with `_t`; the Rust side turns them into
-- styled text (see `src/plugin.rs`, `widget_lines`).

-- Style ---------------------------------------------------------------------

local StyleMT = {}
StyleMT.__index = StyleMT

local function new_style(t)
	return setmetatable(t or { mods = {} }, StyleMT)
end

for _, m in ipairs { "bold", "dim", "italic", "underline", "blink", "blink_rapid", "reverse", "hidden", "crossed" } do
	StyleMT[m] = function(self, on)
		self.mods[m] = on ~= false
		return self
	end
end
StyleMT.reversed = StyleMT.reverse
function StyleMT:fg(c)
	if c == nil then
		return self._fg
	end
	self._fg = c
	return self
end
function StyleMT:bg(c)
	if c == nil then
		return self._bg
	end
	self._bg = c
	return self
end
function StyleMT:patch(o)
	if type(o) ~= "table" then
		return self
	end
	local n = new_style { mods = {} }
	n._fg, n._bg = self._fg, self._bg
	for k, v in pairs(self.mods) do
		n.mods[k] = v
	end
	if o._fg then
		n._fg = o._fg
	end
	if o._bg then
		n._bg = o._bg
	end
	for k, v in pairs(o.mods or {}) do
		n.mods[k] = v
	end
	return n
end
function StyleMT:raw() return { fg = self._fg, bg = self._bg, reversed = self.mods.reverse } end

-- Anything with a style gets the same styling methods.
local function stylable(MT)
	function MT:style(s)
		if s then
			self._style = (self._style or new_style()):patch(s)
		end
		return self
	end
	for _, m in ipairs { "fg", "bg" } do
		MT[m] = function(self, c)
			self._style = self._style or new_style()
			self._style[m](self._style, c)
			return self
		end
	end
	for _, m in ipairs { "bold", "dim", "italic", "underline", "blink", "reverse", "hidden", "crossed" } do
		MT[m] = function(self, on)
			self._style = self._style or new_style()
			self._style[m](self._style, on)
			return self
		end
	end
	function MT:area(a)
		if a == nil then
			return self._area
		end
		self._area = a
		return self
	end
	function MT:align(a)
		self._align = a
		return self
	end
	function MT:wrap(w)
		self._wrap = w
		return self
	end
end

-- Span / Line / Text ----------------------------------------------------------

local SpanMT, LineMT, TextMT = {}, {}, {}
SpanMT.__index, LineMT.__index, TextMT.__index = SpanMT, LineMT, TextMT
stylable(SpanMT)
stylable(LineMT)
stylable(TextMT)

local function text_of(x)
	if type(x) == "table" then
		if x._t == "span" then
			return x.text
		elseif x._t == "line" then
			local parts = {}
			for _, s in ipairs(x.spans) do
				parts[#parts + 1] = text_of(s)
			end
			return table.concat(parts)
		end
		return ""
	end
	return tostring(x == nil and "" or x)
end

local function span(s) return setmetatable({ _t = "span", text = tostring(s == nil and "" or s) }, SpanMT) end
function SpanMT:width() return __ds_width(self.text) end
function SpanMT:visible() return #self.text > 0 end
function SpanMT:truncate(o)
	self.text = __ds_truncate(self.text, o and o.max or #self.text)
	return self
end

local function line(x)
	local spans = {}
	if type(x) == "table" and x._t == nil then
		for _, v in ipairs(x) do
			spans[#spans + 1] = v
		end
	elseif x ~= nil then
		spans[1] = x
	end
	return setmetatable({ _t = "line", spans = spans }, LineMT)
end
function LineMT:width() return __ds_width(text_of(self)) end
function LineMT:visible() return #text_of(self) > 0 end
function LineMT:truncate(o)
	local max = o and o.max or 1e9
	if self:width() > max then
		local s = text_of(self)
		self.spans = { __ds_truncate(s, math.max(0, max - 1)) .. (o.ellipsis or "…") }
	end
	return self
end

local function text(x)
	local lines = {}
	if type(x) == "table" and x._t == nil then
		for _, v in ipairs(x) do
			lines[#lines + 1] = v
		end
	elseif type(x) == "string" then
		for l in (x .. "\n"):gmatch("(.-)\n") do
			lines[#lines + 1] = l
		end
		if #lines > 1 and lines[#lines] == "" then
			lines[#lines] = nil
		end
	elseif x ~= nil then
		lines[1] = x
	end
	return setmetatable({ _t = "text", lines = lines }, TextMT)
end
function TextMT:width()
	local w = 0
	for _, l in ipairs(self.lines) do
		w = math.max(w, __ds_width(text_of(l)))
	end
	return w
end
function TextMT:scroll() return self end

-- List / Table / Row / misc widgets -----------------------------------------

local ListMT, TableMT, RowMT, BarMT = {}, {}, {}, {}
ListMT.__index, TableMT.__index, RowMT.__index, BarMT.__index = ListMT, TableMT, RowMT, BarMT
stylable(ListMT)
stylable(TableMT)
stylable(RowMT)
stylable(BarMT)
function BarMT:title(t)
	self._title = t
	return self
end
function TableMT:widths(w)
	self._widths = w
	return self
end
function TableMT:header(h)
	self._header = h
	return self
end
function TableMT:row(i)
	self._row = i
	return self
end
function TableMT:col(i)
	self._col = i
	return self
end
for _, m in ipairs { "col_style", "cell_style", "row_style", "header_style", "footer", "spacing" } do
	TableMT[m] = function(self, v)
		self["_" .. m] = v
		return self
	end
end
function RowMT:height(h)
	self._height = h
	return self
end
function RowMT:margin() return self end
function BarMT:symbol() return self end
function BarMT:edge() return self end

-- Rect / Pad / Pos / Layout --------------------------------------------------

local RectMT = {}
RectMT.__index = function(r, k)
	if k == "left" then
		return r.x
	elseif k == "right" then
		return r.x + r.w
	elseif k == "top" then
		return r.y
	elseif k == "bottom" then
		return r.y + r.h
	end
	return RectMT[k]
end
RectMT.__call = function(r, o)
	return setmetatable({ x = o.x or r.x, y = o.y or r.y, w = o.w or r.w, h = o.h or r.h }, RectMT)
end
local function rect(t) return setmetatable({ x = t.x or 0, y = t.y or 0, w = t.w or 0, h = t.h or 0 }, RectMT) end
function RectMT:pad(p)
	return rect {
		x = self.x + p.left,
		y = self.y + p.top,
		w = math.max(0, self.w - p.left - p.right),
		h = math.max(0, self.h - p.top - p.bottom),
	}
end
function RectMT:contains(o) return o.x >= self.x and o.y >= self.y and o.x < self.x + self.w and o.y < self.y + self.h end

local Pad = setmetatable({}, {
	__call = function(_, top, right, bottom, left) return { top = top, right = right, bottom = bottom, left = left } end,
})
function Pad.x(n) return Pad(0, n, 0, n) end
function Pad.y(n) return Pad(n, 0, n, 0) end
function Pad.xy(x, y) return Pad(y, x, y, x) end
function Pad.top(n) return Pad(n, 0, 0, 0) end
function Pad.right(n) return Pad(0, n, 0, 0) end
function Pad.bottom(n) return Pad(0, 0, n, 0) end
function Pad.left(n) return Pad(0, 0, 0, n) end

local Constraint = {}
for _, k in ipairs { "Min", "Max", "Length", "Percentage", "Ratio", "Fill" } do
	Constraint[k] = function(a, b) return { kind = k, a = a, b = b } end
end

local LayoutMT = {}
LayoutMT.__index = LayoutMT
function LayoutMT:direction(d)
	self._dir = d
	return self
end
function LayoutMT:constraints(c)
	self._c = c
	return self
end
function LayoutMT:margin() return self end
function LayoutMT:split(area)
	local horizontal = self._dir == 1
	local total = horizontal and area.w or area.h
	local sizes, fills, used = {}, 0, 0
	for i, c in ipairs(self._c or {}) do
		local n
		if c.kind == "Length" or c.kind == "Max" or c.kind == "Min" then
			n = c.a
		elseif c.kind == "Percentage" then
			n = math.floor(total * c.a / 100)
		elseif c.kind == "Ratio" then
			n = math.floor(total * c.a / math.max(1, c.b))
		else
			n = nil
			fills = fills + (c.a or 1)
		end
		sizes[i] = n
		used = used + (n or 0)
	end
	local left = math.max(0, total - used)
	for i, c in ipairs(self._c or {}) do
		if sizes[i] == nil then
			sizes[i] = math.floor(left * (c.a or 1) / math.max(1, fills))
		end
	end
	local all_ratio = #(self._c or {}) > 0
	for _, c in ipairs(self._c or {}) do
		if c.kind ~= "Ratio" then
			all_ratio = false
		end
	end
	if all_ratio then
		local cum, prev = 0, 0
		for i, c in ipairs(self._c) do
			cum = cum + c.a
			local edge = i == #self._c and total or math.floor(total * cum / math.max(1, c.b) + 0.5)
			sizes[i], prev = edge - prev, edge
		end
	end
	local out, pos = {}, horizontal and area.x or area.y
	for i, n in ipairs(sizes) do
		n = math.min(n, (horizontal and area.x + area.w or area.y + area.h) - pos)
		out[i] = horizontal and rect { x = pos, y = area.y, w = n, h = area.h } or rect { x = area.x, y = pos, w = area.w, h = n }
		pos = pos + n
	end
	return out
end

-- ANSI text → ui.Line objects (the Rust side does the parsing).
local function ansi_lines(s)
	local out = {}
	for i, raw in ipairs(__ds_ansi(tostring(s))) do
		local spans = {}
		for j, sp in ipairs(raw) do
			local st = new_style { mods = {} }
			st._fg, st._bg = sp.fg, sp.bg
			for m, on in pairs(sp.mods or {}) do
				st.mods[m] = on
			end
			spans[j] = span(sp.text):style(st)
		end
		out[i] = line(spans)
	end
	return out
end

-- The module ----------------------------------------------------------------

ui = {
	Style = new_style,
	Span = span,
	Line = line,
	Text = setmetatable({ parse = function(s) return text(ansi_lines(s)) end }, { __call = function(_, x) return text(x) end }),
	Paragraph = setmetatable({ parse = function(s) return text(ansi_lines(s)) end }, { __call = function(_, _, x) return text(x) end }),
	List = function(items) return setmetatable({ _t = "list", items = items or {} }, ListMT) end,
	Table = function(rows) return setmetatable({ _t = "table", rows = rows or {} }, TableMT) end,
	Row = function(cells) return setmetatable({ _t = "row", cells = cells or {} }, RowMT) end,
	Bar = function(edge) return setmetatable({ _t = "bar", edge = edge }, BarMT) end,
	Border = setmetatable({ PLAIN = 0, ROUNDED = 1, DOUBLE = 2, THICK = 3, QUADRANT_INSIDE = 4, QUADRANT_OUTSIDE = 5 }, {
		__call = function(_, edge)
			local b = setmetatable({ _t = "border", edge = edge, _type = 0 }, BarMT)
			function b:type(t)
				self._type = t
				return self
			end
			function b:set(name)
				self._type = name == "rounded" and 1 or (name == "double" and 2 or (name == "thick" and 3 or 0))
				return self
			end
			function b:merge()
				self._merge = true
				return self
			end
			return b
		end,
	}),
	Gauge = function()
		local g = setmetatable({ _t = "gauge" }, BarMT)
		function g:percent(p)
			self._p = p
			return self
		end
		function g:ratio(r)
			self._p = r * 100
			return self
		end
		function g:label(l)
			self._label = l
			return self
		end
		function g:gauge_style() return self end
		return g
	end,
	Clear = function(area) return { _t = "clear", _area = area } end,
	Rect = setmetatable({ default = rect {} }, { __call = function(_, t) return rect(t) end }),
	Pad = Pad,
	Pos = function(t) return t end,
	Constraint = Constraint,
	Layout = setmetatable({ HORIZONTAL = 1, VERTICAL = 2 }, {
		__call = function() return setmetatable({ _dir = 2 }, LayoutMT) end,
	}),
	Align = { LEFT = 0, CENTER = 1, RIGHT = 2 },
	Wrap = { NO = 0, YES = 1, TRIM = 2 },
	Edge = { NONE = 0, TOP = 1, RIGHT = 2, BOTTOM = 4, LEFT = 8, ALL = 15 },
	width = function(x) return __ds_width(text_of(x)) end,
	truncate = function(s, o)
		o = o or {}
		local max = o.max or #s
		if __ds_width(s) <= max then
			return s
		end
		if o.rtl then
			return "…" .. __ds_truncate_left(s, max - 1)
		end
		return __ds_truncate(s, math.max(0, max - 1)) .. "…"
	end,
	printable = function(s) return s end,
	render = function() __ds_render() end,
	redraw = function(c) return c and c.redraw and c:redraw() or {} end,
	hide = function() end,
	area = function(name)
		local a = (__ds_areas or {})[name or "preview"]
		return a and rect(a) or rect { x = 0, y = 0, w = 80, h = 24 }
	end,
	lines = function(s) return { s } end,
}

local function style_from(v)
	if type(v) ~= "table" then
		return nil
	end
	local st = new_style { mods = {} }
	st._fg, st._bg = v.fg, v.bg
	for m, on in pairs(v.mods or {}) do
		st.mods[m] = on
	end
	return st
end

-- File methods that need the theme: icons and filetype colours.
local FileMT = __ds_FileMT
function FileMT:icon()
	local i = __ds_icon(tostring(self.url), self.name or "", (self._flags or 0) | (self.is_hovered and 2048 or 0))
	if not i then
		return nil
	end
	return { text = i.text, style = style_from(i.style) or new_style() }
end
function FileMT:style()
	return style_from(__ds_file_style(tostring(self.url), self._mime or "", (self._flags or 0) | (self.is_hovered and 2048 or 0)))
end

-- th: the theme, as ui.Style objects.
th = {}
for section, keys in pairs(__ds_th or {}) do
	th[section] = {}
	for k, v in pairs(keys) do
		if type(v) == "table" then
			local s = new_style { mods = {} }
			s._fg, s._bg = v.fg, v.bg
			for m, on in pairs(v.mods or {}) do
				s.mods[m] = on
			end
			th[section][k] = s
		elseif type(v) == "table" then
			th[section][k] = v
		else
			th[section][k] = v
		end
	end
end
th.icon = { match = function(_, file) return file and FileMT.icon(file) or nil end }

-- Components ----------------------------------------------------------------
-- `children_add` registrations and user-defined methods are rendered by the
-- native UI in the matching slots (see `ui.rs`).

local components = {}

local function component(name, builtins)
	local c = { LEFT = 0, RIGHT = 1, _children = {}, _user = {}, _inc = 1000 }
	function c:children_add(fn, order, side)
		c._inc = c._inc + 1
		table.insert(c._children, { fn = fn, order = order or 9999, side = side or 0, id = c._inc })
		table.sort(c._children, function(a, b) return a.order < b.order end)
		__ds_components_changed()
		return c._inc
	end
	function c:children_remove(id)
		for i, ch in ipairs(c._children) do
			if ch.id == id then
				table.remove(c._children, i)
				break
			end
		end
	end
	if not (builtins and builtins.new) then
		function c:new(...) return setmetatable({ ... }, { __index = c }) end
	end
	components[name] = c
	-- Built-in methods live behind `__index`, so assigning one (`Tab.build =
	-- …`, `function Status:name() … end`) is seen as a user override.
	return setmetatable(c, {
		__index = builtins or {},
		__newindex = function(t, k, v)
			rawset(t, k, v)
			t._user[k] = true
			__ds_components_changed()
		end,
	})
end

local function fmt_time(t)
	t = math.floor(t or 0)
	if t == 0 then
		return ""
	elseif os.date("%Y", t) == os.date("%Y") then
		return os.date("%m/%d %H:%M", t)
	else
		return os.date("%m/%d  %Y", t)
	end
end

Linemode = component("Linemode", {
	new = function(self, file) return setmetatable({ _file = file }, { __index = self }) end,
	solo = function(self)
		if not self._file.in_current then
			return ""
		end
		local mode = cx and cx.active and cx.active.pref.linemode or "none"
		if mode == "none" or mode == "solo" then
			return ""
		elseif not self[mode] then
			return " " .. mode
		end
		local line = ui.Line(self[mode](self))
		return line:visible() and ui.Line { " ", line } or line
	end,
	padding = function(self)
		if not self._file.is_hovered then
			return " "
		end
		local st = Entity.new(Entity, self._file):style_rev()
		return st and ui.Span((th.indicator.padding or {}).close or ""):style(st) or " "
	end,
	redraw = function(self)
		local parts = { { 1000, self:solo() }, { 2000, self:padding() } }
		for _, ch in ipairs(Linemode._children) do
			local ok, w = pcall(ch.fn, self)
			if ok and w then
				parts[#parts + 1] = { ch.order, w }
			end
		end
		table.sort(parts, function(a, b) return a[1] < b[1] end)
		local out = {}
		for i, p in ipairs(parts) do
			out[i] = p[2]
		end
		return ui.Line(out)
	end,
	size = function(self)
		local s = self._file:size()
		return s and ya.readable_size(s) or ""
	end,
	mtime = function(self) return fmt_time(self._file.cha.mtime) end,
	btime = function(self) return fmt_time(self._file.cha.btime) end,
	permissions = function(self) return self._file.cha:perm() or "" end,
	owner = function(self)
		return string.format("%s:%s", ya.user_name(self._file.cha.uid), ya.group_name(self._file.cha.gid))
	end,
})
Status = component("Status", { redraw = function() return nil end })

-- yazi's Entity / Linemode / Current / Parent, used when a plugin overrides
-- `redraw` of a list (the native renderer draws them otherwise).
Entity = component("Entity", {
	new = function(self, file) return setmetatable({ _file = file }, { __index = self }) end,
	padding = function(self)
		if not self._file.is_hovered then
			return " "
		end
		local st = self:style_rev()
		return st and ui.Span((th.indicator.padding or {}).open or ""):style(st) or " "
	end,
	icon = function(self)
		local icon = self._file:icon()
		if not icon then
			return ""
		elseif self._file.is_hovered then
			return icon.text .. " "
		else
			return ui.Line(icon.text .. " "):style(icon.style)
		end
	end,
	prefix = function(self) return "" end,
	highlights = function(self) return self._file.name end,
	found = function(self) return "" end,
	symlink = function(self)
		local to = self._file.link_to
		return (rt.mgr.show_symlink and to) and ui.Span(" -> " .. tostring(to)):style(th.mgr.symlink_target) or ""
	end,
	style = function(self)
		local s = self._file:style() or ui.Style()
		if not self._file.is_hovered then
			return s
		elseif self._file.in_current then
			return s:patch(th.indicator.current)
		elseif self._file.in_preview then
			return s:patch(th.indicator.preview)
		else
			return s:patch(th.indicator.parent)
		end
	end,
	style_rev = function(self)
		local s = self:style()
		local bg = s:bg()
		if bg and bg ~= "reset" then
			return ui.Style():fg(bg):bg("reset"):reverse(true)
		elseif s.mods.reverse then
			return ui.Style():bg("reset"):reverse(true)
		end
	end,
	ellipsis = function(self, max)
		local f = self._file
		return (not f.cha.is_dir and f.url.ext) and ("…." .. f.url.ext) or nil
	end,
	redraw = function(self)
		local lines = {}
		for _, k in ipairs { "padding", "icon", "prefix", "highlights", "found", "symlink" } do
			lines[#lines + 1] = self[k](self)
		end
		for _, ch in ipairs(Entity._children) do
			local ok, w = pcall(ch.fn, self)
			if ok and w then
				lines[#lines + 1] = w
			end
		end
		return ui.Line(lines):style(self:style())
	end,
})
Header = component("Header")
local function list_redraw(self)
	local folder = self._folder
	if not folder then
		return {}
	end
	local left, right = {}, {}
	for _, f in ipairs(folder.window or {}) do
		local entity = Entity:new(f)
		left[#left + 1], right[#right + 1] = entity:redraw(), Linemode:new(f):redraw()
		local max = math.max(0, self._area.w - right[#right]:width())
		left[#left]:truncate { max = max, ellipsis = entity:ellipsis(max) }
	end
	return {
		ui.List(left):area(self._area),
		ui.Text(right):area(self._area):align(ui.Align.RIGHT),
	}
end

Current = component("Current", {
	new = function(self, area, tab)
		return setmetatable({ _area = area, _tab = tab, _folder = tab and tab.current }, { __index = self })
	end,
	empty = function(self)
		return { ui.Text("No items"):area(self._area):align(ui.Align.CENTER) }
	end,
	redraw = function(self)
		if not self._folder or #(self._folder.window or {}) == 0 then
			return self:empty()
		end
		return list_redraw(self)
	end,
})
Parent = component("Parent", {
	new = function(self, area, tab)
		return setmetatable({ _area = area, _tab = tab, _folder = tab and tab.parent }, { __index = self })
	end,
	redraw = list_redraw,
})
Root = component("Root", {
	layout = function(self)
		self._chunks = ui.Layout()
			:direction(ui.Layout.VERTICAL)
			:constraints({
				ui.Constraint.Length(1),
				ui.Constraint.Length(self._tabs or 0),
				ui.Constraint.Fill(1),
				ui.Constraint.Length(1),
			})
			:split(self._area)
	end,
})
Modal = component("Modal")

-- Tab: yazi's own layout/build, so plugins can wrap them (full-border,
-- no-status…). Rust runs these only when a plugin replaced one of them.
Tab = component("Tab", {
	layout = function(self)
		local r = rt.mgr.ratio
		local all = r[1] + r[2] + r[3]
		self._chunks = ui.Layout()
			:direction(ui.Layout.HORIZONTAL)
			:constraints({
				ui.Constraint.Ratio(r[1], all),
				ui.Constraint.Ratio(r[2], all),
				ui.Constraint.Ratio(r[3], all),
			})
			:split(self._area)
	end,
	build = function(self) end,
})
for _, n in ipairs { "Preview", "Tabs", "Rail", "Rails", "Marker", "Markers", "Progress", "Tasks" } do
	_G[n] = component(n)
end

local function self_for(kind, raw)
	if kind == "file" then
		local f = __ds_mk_file(raw)
		f.in_current = raw.in_current ~= false
		f.in_preview = raw.in_preview == true
		f.in_parent = raw.in_parent == true
		return { _file = f }
	end
	local tab = cx and cx.active or {}
	return { _area = rect(raw or {}), _tab = tab, _current = tab.current }
end

-- Rust asks for: user overrides of a named slot, and registered children.
function __ds_component_override(name, method, kind, raw)
	local c = components[name]
	if not c or not c._user[method] then
		return nil
	end
	local self = setmetatable(self_for(kind, raw), { __index = c })
	local ok, w = pcall(c[method], self)
	if not ok then
		ya.err(w)
		return nil
	end
	return w
end

function __ds_component_children(name, kind, raw, side)
	local c = components[name]
	if not c or #c._children == 0 then
		return nil
	end
	local self = setmetatable(self_for(kind, raw), { __index = c })
	local out = {}
	for _, ch in ipairs(c._children) do
		if side == nil or ch.side == side then
			local ok, w = pcall(ch.fn, self)
			if ok and w ~= nil and w ~= "" then
				out[#out + 1] = { ch.order, w }
			elseif not ok then
				ya.err(w)
			end
		end
	end
	return out
end

function __ds_component_active(name)
	local c = components[name]
	if not c then
		return false
	end
	return #c._children > 0 or next(c._user) ~= nil
end

function __ds_has_method(name, method)
	local c = components[name]
	return c ~= nil and type(rawget(c, method)) == "function"
end

-- The Tab layout when a plugin wrapped `Tab.layout` / `Tab.build`.
function __ds_tab_layout(x, y, w, h)
	local self = setmetatable({ _area = rect { x = x, y = y, w = w, h = h }, _tab = cx and cx.active }, { __index = Tab })
	self:layout()
	self:build()
	local out = { area = self._area, chunks = self._chunks, borders = {} }
	for _, b in ipairs(self._base or {}) do
		if type(b) == "table" and b._t == "border" and b._area then
			out.borders[#out.borders + 1] = { area = b._area, type = b._type or 0, merge = b._merge == true, style = b._style }
		end
	end
	return out
end

function __ds_status_hidden()
	if not Status._user.redraw then
		return false
	end
	local ok, r = pcall(Status.redraw, setmetatable({}, { __index = Status }))
	return ok and type(r) == "table" and next(r) == nil
end

function __ds_tab_custom() return Tab._user.layout == true or Tab._user.build == true end

-- Rust: a list component's redraw when a plugin overrode it.
function __ds_list_custom(name)
	local c = components[name]
	return c ~= nil and c._user.redraw == true
end

function __ds_list_redraw(name, x, y, w, h)
	local c = components[name]
	local self = c:new(rect { x = x, y = y, w = w, h = h }, cx and cx.active)
	local ok, r = pcall(c.redraw, self)
	if not ok then
		ya.err(r)
		return {}
	end
	return r or {}
end

function __ds_root_custom() return Root._user.layout == true end

function __ds_root_layout(w, h, tabs)
	local self = setmetatable({ _area = rect { x = 0, y = 0, w = w, h = h }, _tabs = tabs }, { __index = Root })
	self:layout()
	return self._chunks
end

-- Rust: what Modal children (plugin UIs like `mount`) draw this frame.
function __ds_modal_draw(w, h)
	local out = {}
	for _, ch in ipairs(Modal._children) do
		local o = ch.fn
		local inst = o
		if type(o) == "table" and type(o.new) == "function" then
			local ok, r = pcall(o.new, o, rect { x = 0, y = 0, w = w, h = h })
			inst = ok and r or o
		end
		local ok, ws = pcall(function()
			if type(inst) == "function" then
				return inst()
			end
			return inst:redraw()
		end)
		if ok and type(ws) == "table" then
			if ws._t then
				out[#out + 1] = ws
			else
				for _, w2 in ipairs(ws) do
					out[#out + 1] = w2
				end
			end
		elseif not ok then
			ya.err(ws)
		end
	end
	return out
end

function __ds_modal_active() return #Modal._children > 0 end
