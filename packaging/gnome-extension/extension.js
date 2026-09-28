import {API} from './client.js';
import {Indicator} from './indicator.js';
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
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
const XML = `<node><interface name="${BUS}"><method name="Call"><arg type="s" direction="in"/><arg type="s" direction="out"/></method><signal name="FocusChanged"><arg type="b"/></signal><signal name="EdgeHit"><arg type="s"/><arg type="u"/></signal></interface></node>`;
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
        this._indicator = new Indicator();
        this._lease = null;
        this._barriers = [];
        // Edges that lead to another computer, and their barriers.
        this._edges = [];
        this._outbound = [];
        this._hidden = false;
        this._idles = new Set();
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
            (_connection, _name, owner) => { this._agent = owner; this._sendFocus(); }, () => { this._agent = null; });
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
            this._placeEdges();
        });
    }

    disable() {
        this._indicator?.destroy();
        this._indicator = null;
        this._clear();
        this._edges = [];
        this._placeEdges();
        this._setHidden(false);
        // An entry still waiting for the cursor stops here and never answers.
        for (const id of this._idles) GLib.Source.remove(id);
        this._idles.clear();
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
        if (this._lease)
            for (const reply of this._lease.polls) reply(new Error('Desktop handoff expired or ended'));
        for (const barrier of this._barriers) barrier.destroy();
        this._barriers = [];
        this._lease = null;
    }

    _placeEdges() {
        for (const barrier of this._outbound) barrier.destroy();
        this._outbound = [];
        const ms = Main.layoutManager.monitors;
        if (!this._edges.length || !ms.length || (global.backend.capabilities & Meta.BackendCapabilities.BARRIERS) === 0)
            return;
        for (const {edge, start, end} of this._edges) {
            const g = edgeGeometry(ms, edge, start, end);
            const [low, high] = g.vertical ? [g.top, g.bottom] : [g.left, g.right];
            const segments = g.segments.map(s => ({...s,
                start: s.start === low ? s.start + DEAD_CORNER : s.start,
                end: s.end === high ? s.end - DEAD_CORNER : s.end,
            })).filter(s => s.end > s.start);
            for (const segment of segments) {
                const barrier = edgeBarrier(edge, g, segment);
                // Shell sends a hit for every motion against the barrier; report
                // each push once. While another computer controls this one, its
                // own return barrier on this edge answers instead.
                let push = null;
                barrier.connect('hit', (_barrier, event) => {
                    if (this._lease || !this._agent || event.event_id === push) return;
                    push = event.event_id;
                    const axis = g.vertical ? event.y : event.x;
                    const position = Math.max(start, Math.min(end, Math.round((axis - g.origin) * MAX / g.span)));
                    Gio.DBus.session.emit_signal(this._agent, PATH, BUS, 'EdgeHit', new GLib.Variant('(su)', [edge, position]));
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

    _snapshot() {
        // Shell disables this extension while locked, so only the monitors need checking.
        if (!Main.layoutManager.monitors.length) throw new Error('GNOME has no active monitors');
        const monitors = Main.layoutManager.monitors.map(m => ({x: m.x, y: m.y, width: m.width, height: m.height}));
        if (monitors.length > 16) throw new Error('At most 16 monitors are supported');
        const [x, y] = global.get_pointer();
        return {geometry: {monitors}, position: {x, y}};
    }

    async CallAsync([json], invocation) {
        if (invocation.get_sender() !== this._agent) {
            invocation.return_dbus_error('org.freedesktop.DBus.Error.AccessDenied', 'Only the zflow desktop agent may call this');
            return;
        }
        let response;
        try {
            if (json.length > 4096) throw new Error('Desktop request exceeds limit');
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
            if (!Array.isArray(r.edges) || r.edges.length > 64
                || !r.edges.every(e => EDGES.includes(e?.edge) && validRange(e.start, e.end)))
                throw new Error('Invalid outbound edges');
            this._edges = r.edges.map(({edge, start, end}) => ({edge, start, end}));
            this._placeEdges();
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
        const g = edgeGeometry(snapshot.geometry.monitors, r.edge, r.start, r.end);
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
            // GNOME 51 removed Clutter.get_default_backend().
            const backend = global.stage.get_context?.().get_backend() ?? Clutter.get_default_backend();
            backend.get_default_seat().warp_pointer(point.x, point.y);
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
                if (Math.abs(actual.position.x - point.x) <= 2 && Math.abs(actual.position.y - point.y) <= 2)
                    return {status: 'prepared', ...actual};
                if (GLib.get_monotonic_time() >= deadline)
                    throw new Error('GNOME did not place the cursor at the requested entry');
            }
        } catch (error) {
            if (this._lease === lease) this._clear();
            throw error;
        }
    }
}
