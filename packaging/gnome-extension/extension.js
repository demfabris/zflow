import {API} from './client.js';
import {Indicator} from './indicator.js';
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const BUS = 'org.gnome.Shell.Extensions.Zflow';
const PATH = '/org/gnome/Shell/Extensions/Zflow';
// src/app/gnome.rs owns this name on the connection that calls us.
const AGENT = 'io.zflow.Desktop';
const MAX = 1000000;
const LEASE_US = 2000000;
const WARP_US = 100000;
// src/desktop.rs mirrors this hold duration.
const POLL_HOLD_MS = 200;
// ReadClipboard answers one kind: "text", "png", "empty", or
// "too_large:<bytes>" for a clip over MAX_CLIP_BYTES, whose data stays here.
const XML = `<node><interface name="${BUS}"><method name="Call"><arg type="s" direction="in"/><arg type="s" direction="out"/></method><method name="ReadClipboard"><arg name="kind" type="s" direction="out"/><arg name="data" type="ay" direction="out"/></method><method name="WriteClipboard"><arg name="kind" type="s" direction="in"/><arg name="data" type="ay" direction="in"/></method><signal name="FocusChanged"><arg type="b"/></signal><signal name="EdgeHit"><arg type="s"/><arg type="s"/><arg type="u"/></signal></interface></node>`;
// src/clipboard.rs mirrors this limit.
const MAX_CLIP_BYTES = 3 * 1024 * 1024;
// The app that copied hands the clipboard over, and a stuck one never does.
const CLIP_WAIT_MS = 1000;
// One kind per clip: text if the clipboard has any, else a PNG.
const CLIP_TYPES = [['text/plain;charset=utf-8', 'text'], ['text/plain', 'text'], ['image/png', 'png']];
const NO_BYTES = new Uint8Array();
const EDGES = ['left', 'right', 'top', 'bottom'];
// Outbound barriers stop this far from the desktop's corners, so a push into
// a corner, such as GNOME's hot corner, never crosses.
const DEAD_CORNER = 8;

// The monitors along one outer edge of the desktop, cut to a range given as
// fractions of MAX along that edge.
function edgeGeometry(ms, edge, start, end) {
    const left = Math.min(...ms.map(m => m.x));
    const top = Math.min(...ms.map(m => m.y));
    const right = Math.max(...ms.map(m => m.x + m.width));
    const bottom = Math.max(...ms.map(m => m.y + m.height));
    const vertical = edge === 'left' || edge === 'right';
    const origin = vertical ? top : left;
    const span = vertical ? bottom - top : right - left;
    const boundary = {left, right, top, bottom}[edge];
    const segments = ms.filter(m => ({left: m.x, right: m.x + m.width, top: m.y, bottom: m.y + m.height}[edge]) === boundary)
        .map(m => ({
            start: Math.max(vertical ? m.y : m.x, origin + Math.ceil(start * span / MAX)),
            end: Math.min(vertical ? m.y + m.height : m.x + m.width, origin + Math.floor(end * span / MAX)),
            monitor: m,
        })).filter(s => s.end > s.start);
    return {left, top, right, bottom, vertical, origin, span, boundary, segments};
}

// A barrier on an outer edge that lets the pointer back in but not out, so
// Shell reports each push against it.
function edgeBarrier(edge, g, segment) {
    const directions = {left: Meta.BarrierDirection.POSITIVE_X, right: Meta.BarrierDirection.NEGATIVE_X,
        top: Meta.BarrierDirection.POSITIVE_Y, bottom: Meta.BarrierDirection.NEGATIVE_Y};
    return new Meta.Barrier({backend: global.backend, directions: directions[edge],
        x1: g.vertical ? g.boundary : segment.start, x2: g.vertical ? g.boundary : segment.end,
        y1: g.vertical ? segment.start : g.boundary, y2: g.vertical ? segment.end : g.boundary});
}

function validRange(start, end) {
    return [start, end].every(Number.isSafeInteger) && start >= 0 && start < end && end <= MAX;
}

export default class ZflowExtension extends Extension {
    enable() {
        this._displayInfo = null;
        this._displayGeneration = 0;
        this._displayCall = null;
        this._indicator = new Indicator();
        this._lease = null;
        this._barriers = [];
        // Edges that lead to another computer, and their barriers.
        this._edges = [];
        this._pauseMs = 0;
        this._outbound = [];
        // Pushes waiting out the pause before they cross.
        this._waits = new Set();
        this._hidden = false;
        this._idles = new Set();
        // Clipboard reads waiting for the app that copied.
        this._reads = new Set();
        // Only the desktop agent may read the pointer or move it. This skips GNOME's
        // DBusSenderChecker, whose destroy() passes array indexes to unwatch_name.
        this._agent = null;
        // Whether the focused window is a terminal, so Mac shortcuts can use
        // Ctrl+Shift there. Only the agent hears about it.
        this._terminal = this._focusedTerminal();
        this._focusId = global.display.connect('notify::focus-window', () => {
            const terminal = this._focusedTerminal();
            if (terminal === this._terminal) return;
            this._terminal = terminal;
            this._sendFocus();
        });
        this._agentWatch = Gio.bus_watch_name_on_connection(Gio.DBus.session, AGENT, Gio.BusNameWatcherFlags.NONE,
            (_connection, _name, owner) => { this._agent = owner; this._sendFocus(); },
            () => {
                // Nobody is left to hear a push or to show the pointer again.
                this._agent = null;
                this._edges = [];
                this._placeEdges();
                this._setHidden(false);
            });
        this._object = Gio.DBusExportedObject.wrapJSObject(XML, this);
        this._object.export(Gio.DBus.session, PATH);
        this._busId = Gio.bus_own_name_on_connection(Gio.DBus.session, BUS, Gio.BusNameOwnerFlags.NONE, null, null);
        this._timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 250, () => {
            if (this._lease && GLib.get_monotonic_time() - this._lease.renewed >= LEASE_US)
                this._clear();
            return GLib.SOURCE_CONTINUE;
        });
        this._monitorsId = Main.layoutManager.connect('monitors-changed', () => {
            this._clear();
            this._refreshDisplays();
        });
        this._refreshDisplays();
    }

    disable() {
        ++this._displayGeneration;
        this._displayCall?.cancel();
        this._displayCall = null;
        this._displayInfo = null;
        this._indicator?.destroy();
        this._indicator = null;
        this._edges = [];
        this._clear();
        this._placeEdges();
        this._setHidden(false);
        // An entry still waiting for the cursor stops here and never answers.
        for (const id of this._idles) GLib.Source.remove(id);
        this._idles.clear();
        for (const read of [...this._reads]) read.finish(null, new Error('The zflow extension turned off'));
        if (this._timer) GLib.Source.remove(this._timer);
        if (this._monitorsId) Main.layoutManager.disconnect(this._monitorsId);
        if (this._focusId) global.display.disconnect(this._focusId);
        this._timer = this._monitorsId = this._focusId = 0;
        this._object?.unexport();
        this._object = null;
        if (this._busId) Gio.bus_unown_name(this._busId);
        if (this._agentWatch) Gio.bus_unwatch_name(this._agentWatch);
        this._busId = this._agentWatch = 0;
    }

    _clear() {
        const leased = this._lease !== null;
        if (this._lease)
            for (const reply of this._lease.polls) reply(new Error('Desktop handoff expired or ended'));
        for (const barrier of this._barriers) barrier.destroy();
        this._barriers = [];
        this._lease = null;
        // This computer's own edges come back once nobody controls it.
        if (leased) this._placeEdges();
    }

    _placeEdges() {
        for (const barrier of this._outbound) barrier.destroy();
        this._outbound = [];
        for (const id of this._waits) GLib.Source.remove(id);
        this._waits.clear();
        const ms = Main.layoutManager.monitors;
        // While another computer controls this one, it has no edges of its own.
        if (this._lease || !this._edges.length || !ms.length || !this._displayInfo
            || (global.backend.capabilities & Meta.BackendCapabilities.BARRIERS) === 0)
            return;
        for (const {monitor, edge, start, end} of this._edges) {
            let selected;
            try { selected = this._selected(this._snapshot().geometry, monitor); }
            catch { continue; } // A disconnected monitor has no barrier.
            const g = edgeGeometry(selected, edge, start, end);
            const [low, high] = g.vertical ? [g.top, g.bottom] : [g.left, g.right];
            const segments = g.segments.map(s => ({...s,
                start: s.start === low ? s.start + DEAD_CORNER : s.start,
                end: s.end === high ? s.end - DEAD_CORNER : s.end,
            })).filter(s => s.end > s.start);
            for (const segment of segments) {
                const barrier = edgeBarrier(edge, g, segment);
                // Shell sends a hit for every motion against the barrier; report
                // each push once.
                let push = null;
                let latest = 0;
                let waiting = 0;
                const report = position => {
                    if (!this._lease && this._agent)
                        Gio.DBus.session.emit_signal(this._agent, PATH, BUS, 'EdgeHit', new GLib.Variant('(ssu)', [monitor ?? '', edge, position]));
                };
                barrier.connect('hit', (_barrier, event) => {
                    if (this._lease || !this._agent) return;
                    const axis = g.vertical ? event.y : event.x;
                    latest = Math.max(start, Math.min(end, Math.round((axis - g.origin) * MAX / g.span)));
                    if (event.event_id === push) return;
                    push = event.event_id;
                    if (!this._pauseMs) {
                        report(latest);
                        return;
                    }
                    // The pointer has to rest here a moment; leaving cancels it.
                    waiting = GLib.timeout_add(GLib.PRIORITY_DEFAULT, this._pauseMs, () => {
                        this._waits.delete(waiting);
                        waiting = 0;
                        report(latest);
                        return GLib.SOURCE_REMOVE;
                    });
                    this._waits.add(waiting);
                });
                barrier.connect('left', () => {
                    if (!waiting) return;
                    GLib.Source.remove(waiting);
                    this._waits.delete(waiting);
                    waiting = 0;
                });
                this._outbound.push(barrier);
            }
        }
    }

    // Balanced with the Shell's cursor tracker, which counts inhibitions.
    _setHidden(hidden) {
        if (hidden === this._hidden) return;
        const tracker = global.backend.get_cursor_tracker();
        if (hidden) tracker.inhibit_cursor_visibility();
        else tracker.uninhibit_cursor_visibility();
        this._hidden = hidden;
    }

    _focusedTerminal() {
        const win = global.display.focus_window;
        const app = win && Shell.WindowTracker.get_default().get_window_app(win);
        const categories = app?.get_app_info()?.get_categories() ?? '';
        return categories.split(';').includes('TerminalEmulator');
    }

    _sendFocus() {
        if (this._agent)
            Gio.DBus.session.emit_signal(this._agent, PATH, BUS, 'FocusChanged', new GLib.Variant('(b)', [this._terminal]));
    }

    _refreshDisplays() {
        this._displayCall?.cancel();
        const generation = ++this._displayGeneration;
        this._displayInfo = null;
        this._placeEdges();
        const cancellable = new Gio.Cancellable();
        this._displayCall = cancellable;
        // This service runs inside Shell too. An asynchronous call avoids
        // blocking the compositor while it answers its own display query.
        Gio.DBus.session.call('org.gnome.Mutter.DisplayConfig', '/org/gnome/Mutter/DisplayConfig',
            'org.gnome.Mutter.DisplayConfig', 'GetCurrentState', null, null,
            Gio.DBusCallFlags.NONE, 2000, cancellable, (connection, result) => {
                if (generation !== this._displayGeneration) return;
                this._displayCall = null;
                try {
                    const [, physical, logical] = connection.call_finish(result).deep_unpack();
                    const unpack = value => value?.deep_unpack ? value.deep_unpack() : value;
                    this._displayInfo = logical.map(([x, y, , transform, , specs]) => {
                        // Mirrors are one logical surface. Pick a stable member.
                        const spec = [...specs].sort((a,b) => JSON.stringify(a).localeCompare(JSON.stringify(b)))[0];
                        const properties = physical.find(([candidate]) => JSON.stringify(candidate) === JSON.stringify(spec))?.[2] ?? {};
                        const index = global.backend.get_monitor_manager?.().get_monitor_for_connector(spec[0]);
                        let width = unpack(properties['width-mm']) ?? 0;
                        let height = unpack(properties['height-mm']) ?? 0;
                        if (transform % 2) [width, height] = [height, width];
                        if (width < 10 || height < 10 || width > 4000 || height > 4000) width = height = 0;
                        return {x, y, index, id: GLib.compute_checksum_for_string(GLib.ChecksumType.SHA256, JSON.stringify(spec), -1),
                            name: String(unpack(properties['display-name']) ?? spec[0]).replace(/[\x00-\x1f\x7f]/g, '').slice(0, 32),
                            width_mm: width, height_mm: height, active: true};
                    });
                    this._placeEdges();
                } catch { this._displayInfo = null; }
            });
    }

    _selected(geometry, id) {
        if (id === undefined || id === null) return geometry.monitors;
        const display = geometry.displays.find(d => d.id === id);
        if (!display) throw new Error('The selected monitor is disconnected; refresh the screen arrangement');
        return [display.bounds];
    }

    _snapshot() {
        // Shell disables this extension while locked, so only the monitors need checking.
        if (!Main.layoutManager.monitors.length) throw new Error('GNOME has no active monitors');
        const monitors = Main.layoutManager.monitors.map(m => ({x: m.x, y: m.y, width: m.width, height: m.height}));
        if (monitors.length > 16) throw new Error('At most 16 monitors are supported');
        if (!this._displayInfo) {
            if (!this._displayCall) this._refreshDisplays();
            throw new Error('Waiting for GNOME monitor information');
        }
        const displays = monitors.map((bounds, index) => {
            const info = this._displayInfo.find(d => d.index === undefined ? d.x === bounds.x && d.y === bounds.y : d.index === index);
            if (!info) throw new Error('GNOME monitors changed; waiting for an updated screen arrangement');
            const {x: _x, y: _y, index: _index, ...metadata} = info;
            return {...metadata, bounds};
        });
        const [x, y] = global.get_pointer();
        return {geometry: {monitors, displays}, position: {x, y}};
    }

    // Only the desktop agent may call in.
    _fromAgent(invocation) {
        if (invocation.get_sender() === this._agent) return true;
        invocation.return_dbus_error('org.freedesktop.DBus.Error.AccessDenied', 'Only the zflow desktop agent may call this');
        return false;
    }

    async ReadClipboardAsync(_params, invocation) {
        if (!this._fromAgent(invocation)) return;
        try {
            const [kind, data] = await this._readClipboard();
            invocation.return_value(new GLib.Variant('(say)', [kind, data]));
        } catch (error) {
            invocation.return_dbus_error('org.freedesktop.DBus.Error.Failed', String(error.message).slice(0, 256));
        }
    }

    async WriteClipboardAsync([kind, data], invocation) {
        if (!this._fromAgent(invocation)) return;
        try {
            this._writeClipboard(kind, data);
            invocation.return_value(null);
        } catch (error) {
            invocation.return_dbus_error('org.freedesktop.DBus.Error.InvalidArgs', String(error.message).slice(0, 256));
        }
    }

    async _readClipboard() {
        const offered = St.Clipboard.get_default().get_mimetypes(St.ClipboardType.CLIPBOARD) ?? [];
        const [mimetype, kind] = CLIP_TYPES.find(([type]) => offered.includes(type)) ?? [];
        const {size, data} = mimetype ? await this._clipboardContent(mimetype) : {size: 0};
        if (!size) return ['empty', NO_BYTES];
        if (size > MAX_CLIP_BYTES) return [`too_large:${size}`, NO_BYTES];
        return [kind, data];
    }

    // St hands the bytes over once the app that copied sends them all, and
    // frees them as soon as the callback returns, so they are copied out there.
    _clipboardContent(mimetype) {
        return new Promise((resolve, reject) => {
            const read = {
                finish: (clip, error) => {
                    if (!this._reads.delete(read)) return;
                    if (read.timer) GLib.Source.remove(read.timer);
                    if (error) reject(error);
                    else resolve(clip);
                },
            };
            read.timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, CLIP_WAIT_MS, () => {
                read.timer = 0;
                read.finish(null, new Error('The app that copied did not hand the clipboard over'));
                return GLib.SOURCE_REMOVE;
            });
            this._reads.add(read);
            St.Clipboard.get_default().get_content(St.ClipboardType.CLIPBOARD, mimetype, (_clipboard, bytes) => {
                const size = bytes?.get_size() ?? 0;
                read.finish({size, data: size && size <= MAX_CLIP_BYTES ? bytes.toArray() : NO_BYTES});
            });
        });
    }

    _writeClipboard(kind, data) {
        if (!data?.length || data.length > MAX_CLIP_BYTES) throw new Error('Invalid clip');
        const clipboard = St.Clipboard.get_default();
        if (kind === 'text')
            clipboard.set_text(St.ClipboardType.CLIPBOARD, new TextDecoder('utf-8', {fatal: true}).decode(data));
        else if (kind === 'png')
            clipboard.set_content(St.ClipboardType.CLIPBOARD, 'image/png', new GLib.Bytes(data));
        else
            throw new Error('Unknown clip kind');
    }

    async CallAsync([json], invocation) {
        if (!this._fromAgent(invocation)) return;
        let response;
        try {
            if (json.length > 32768) throw new Error('Desktop request exceeds limit');
            const request = JSON.parse(json);
            // Agents older than API 1 did not send it and speak API 1.
            const api = request.api ?? 1;
            if (api > API) throw new Error('Update zflow: its GNOME extension is older than the app');
            if (api < API) throw new Error('Update zflow: the app is older than its GNOME extension');
            response = await this._request(request);
        } catch (error) {
            response = {status: 'unavailable', reason: String(error.message).slice(0, 256)};
        }
        invocation.return_value(new GLib.Variant('(s)', [JSON.stringify(response)]));
    }

    async _request(r) {
        // The agent asks for this when it subscribes; it needs no monitors.
        if (r.command === 'focus') return {status: 'focus', terminal: this._terminal};
        // This computer's own input; only the service sends these.
        if (r.command === 'sending') {
            if (typeof r.active !== 'boolean') throw new Error('Invalid sending state');
            this._setHidden(r.active);
            return {status: 'finished'};
        }
        if (r.command === 'edges') {
            const pauseMs = r.pause_ms ?? 0;
            if (!Array.isArray(r.edges) || r.edges.length > 64
                || !r.edges.every(e => EDGES.includes(e?.edge) && validRange(e.start, e.end)
                    && (e.monitor === undefined || (typeof e.monitor === 'string' && e.monitor.length > 0 && e.monitor.length <= 128)))
                || !Number.isSafeInteger(pauseMs) || pauseMs < 0 || pauseMs > 2000)
                throw new Error('Invalid outbound edges');
            const edges = r.edges.map(({monitor, edge, start, end}) => ({monitor, edge, start, end}));
            // Rebuilding would forget the push in progress, so a pointer still
            // resting on the barrier after a return would cross again.
            if (JSON.stringify(edges) !== JSON.stringify(this._edges) || pauseMs !== this._pauseMs) {
                this._edges = edges;
                this._pauseMs = pauseMs;
                this._placeEdges();
            }
            return {status: 'finished'};
        }
        if (r.command === 'notify') {
            if (typeof r.message !== 'string' || !r.message) throw new Error('Invalid notice');
            Main.notify('zflow', r.message.slice(0, 256));
            return {status: 'finished'};
        }
        const snapshot = this._snapshot();
        if (r.command === 'warp') {
            const {x, y} = r.position ?? {};
            if (!snapshot.geometry.monitors.some(m => x >= m.x && y >= m.y && x < m.x + m.width && y < m.y + m.height))
                throw new Error('The pointer cannot go outside the monitors');
            (global.stage.get_context?.().get_backend() ?? Clutter.get_default_backend()).get_default_seat().warp_pointer(x, y);
            return {status: 'finished'};
        }
        if (r.command === 'snapshot') return {status: 'snapshot', ...snapshot};
        if (!Number.isSafeInteger(r.token) || r.token <= 0) throw new Error('Invalid handoff token');
        if (this._lease && GLib.get_monotonic_time() - this._lease.renewed >= LEASE_US)
            this._clear();
        if (r.command === 'prepare') return this._prepare(r, snapshot);
        if (!this._lease || this._lease.token !== r.token) {
            throw new Error('Desktop handoff expired or ended');
        }
        if (r.command === 'finish') {
            this._clear();
            return {status: 'finished'};
        }
        if (r.command !== 'poll') throw new Error('Unknown desktop operation');
        this._lease.renewed = GLib.get_monotonic_time();
        const lease = this._lease;
        if (lease.returned !== null) return {status: 'returned', position: lease.returned};
        return new Promise((resolve, reject) => {
            const reply = error => {
                GLib.Source.remove(timer);
                lease.polls.delete(reply);
                if (error) reject(error);
                else resolve({status: 'returned', position: lease.returned});
            };
            const timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, POLL_HOLD_MS, () => {
                lease.polls.delete(reply);
                resolve({status: 'active'});
                return GLib.SOURCE_REMOVE;
            });
            lease.polls.add(reply);
        });
    }

    async _prepare(r, snapshot) {
        if (this._lease) throw new Error('A desktop handoff is already active');
        if (!validRange(r.start, r.end) || !Number.isSafeInteger(r.position) || r.position < r.start || r.position > r.end)
            throw new Error('Invalid crossing range');
        if (!EDGES.includes(r.edge)) throw new Error('Invalid edge');
        if ((global.backend.capabilities & Meta.BackendCapabilities.BARRIERS) === 0)
            throw new Error('This GNOME session does not provide pointer barriers');
        const g = edgeGeometry(this._selected(snapshot.geometry, r.monitor), r.edge, r.start, r.end);
        const {left, top, right, bottom, vertical, origin, span, segments} = g;
        // The Mac tile is this desktop's bounding box, so the entry can fall where no
        // monitor touches the edge, or a rounding pixel outside the range. Enter at
        // the nearest pixel that has a monitor behind it.
        const wanted = origin + Math.floor(r.position * span / MAX);
        const along = s => Math.max(s.start, Math.min(s.end - 1, wanted));
        const segment = segments.reduce((best, s) =>
            best && Math.abs(along(best) - wanted) <= Math.abs(along(s) - wanted) ? best : s, null);
        if (!segment) throw new Error('No GNOME monitor touches the selected crossing range');
        const coordinate = along(segment);
        const m = segment.monitor;
        if (m.width < 8 || m.height < 8) throw new Error('The entry monitor is too small');
        const point = vertical ? {x: r.edge === 'left' ? left + 3 : right - 4, y: coordinate}
            : {x: coordinate, y: r.edge === 'top' ? top + 3 : bottom - 4};
        const lease = {token: r.token, renewed: GLib.get_monotonic_time(), returned: null, polls: new Set()};
        this._lease = lease;
        try {
            // Mutter runs a warp through the pointer barriers like any motion,
            // and a barrier the pointer rests on pins it to the edge when the
            // warp also moves along it, as after a return through this edge.
            // So this computer's own barriers go first, and the return barriers
            // go up only once the pointer is in.
            this._placeEdges();
            // GNOME 51 removed Clutter.get_default_backend().
            const backend = global.stage.get_context?.().get_backend() ?? Clutter.get_default_backend();
            backend.get_default_seat().warp_pointer(point.x, point.y);
            // Mutter applies the warp on its input thread, and a slow frame can
            // delay it past the next main-loop turn. Keep checking for a while.
            const deadline = GLib.get_monotonic_time() + WARP_US;
            for (;;) {
                await new Promise(resolve => {
                    const id = GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
                        this._idles.delete(id);
                        resolve();
                        return GLib.SOURCE_REMOVE;
                    });
                    this._idles.add(id);
                });
                if (this._lease !== lease) throw new Error('Desktop changed during entry');
                const actual = this._snapshot();
                if (Math.abs(actual.position.x - point.x) <= 2 && Math.abs(actual.position.y - point.y) <= 2) {
                    for (const s of segments) {
                        const barrier = edgeBarrier(r.edge, g, s);
                        barrier.connect('hit', (_barrier, event) => {
                            if (this._lease !== lease || lease.returned !== null) return;
                            const axis = vertical ? event.y : event.x;
                            lease.returned = Math.max(r.start, Math.min(r.end, Math.round((axis - origin) * MAX / span)));
                            for (const reply of lease.polls) reply();
                        });
                        this._barriers.push(barrier);
                    }
                    return {status: 'prepared', ...actual};
                }
                if (GLib.get_monotonic_time() >= deadline) {
                    const {x, y} = actual.position;
                    throw new Error(`GNOME did not place the cursor at the requested entry: asked for ${point.x},${point.y} and it is at ${x},${y}`);
                }
            }
        } catch (error) {
            if (this._lease === lease) this._clear();
            throw error;
        }
    }
}
