import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const BUS = 'org.gnome.Shell.Extensions.Zflow';
const PATH = '/org/gnome/Shell/Extensions/Zflow';
const MAX = 1000000;
const LEASE_US = 2000000;
// src/desktop.rs mirrors this hold duration.
const POLL_HOLD_MS = 200;
const XML = `<node><interface name="${BUS}"><method name="Call"><arg type="s" direction="in"/><arg type="s" direction="out"/></method></interface></node>`;

export default class ZflowExtension extends Extension {
    enable() {
        this._lease = null;
        this._barriers = [];
        this._object = Gio.DBusExportedObject.wrapJSObject(XML, this);
        this._object.export(Gio.DBus.session, PATH);
        this._busId = Gio.bus_own_name_on_connection(Gio.DBus.session, BUS, Gio.BusNameOwnerFlags.NONE, null, null);
        this._timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 250, () => {
            if (this._lease && (GLib.get_monotonic_time() - this._lease.renewed >= LEASE_US || !this._available()))
                this._clear();
            return GLib.SOURCE_CONTINUE;
        });
        this._monitorsId = Main.layoutManager.connect('monitors-changed', () => this._clear());
    }

    disable() {
        this._clear();
        if (this._timer) GLib.Source.remove(this._timer);
        if (this._monitorsId) Main.layoutManager.disconnect(this._monitorsId);
        this._timer = this._monitorsId = 0;
        this._object?.unexport();
        this._object = null;
        if (this._busId) Gio.bus_unown_name(this._busId);
        this._busId = 0;
    }

    _available() {
        return !Main.sessionMode.isLocked && !Main.sessionMode.isGreeter && Main.layoutManager.monitors.length > 0;
    }

    _clear() {
        if (this._lease)
            for (const reply of this._lease.polls) reply(new Error('Desktop handoff expired or ended'));
        for (const barrier of this._barriers) barrier.destroy();
        this._barriers = [];
        this._lease = null;
    }

    _snapshot() {
        if (!this._available()) throw new Error('Unlock the local GNOME session to receive input');
        const monitors = Main.layoutManager.monitors.map(m => ({x: m.x, y: m.y, width: m.width, height: m.height}));
        if (monitors.length > 16) throw new Error('At most 16 monitors are supported');
        const [x, y] = global.get_pointer();
        return {geometry: {monitors}, position: {x, y}};
    }

    async CallAsync([json], invocation) {
        let response;
        try {
            if (json.length > 4096) throw new Error('Desktop request exceeds limit');
            const request = JSON.parse(json);
            response = await this._request(request);
        } catch (error) {
            response = {status: 'unavailable', reason: String(error.message).slice(0, 256)};
        }
        invocation.return_value(new GLib.Variant('(s)', [JSON.stringify(response)]));
    }

    async _request(r) {
        const snapshot = this._snapshot();
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
        if (![r.start, r.end, r.position].every(Number.isSafeInteger) || r.start < 0 || r.start >= r.end || r.end > MAX || r.position < r.start || r.position > r.end)
            throw new Error('Invalid crossing range');
        if (!['left', 'right', 'top', 'bottom'].includes(r.edge)) throw new Error('Invalid edge');
        if ((global.backend.capabilities & Meta.BackendCapabilities.BARRIERS) === 0)
            throw new Error('This GNOME session does not provide pointer barriers');
        const ms = snapshot.geometry.monitors;
        const left = Math.min(...ms.map(m => m.x));
        const top = Math.min(...ms.map(m => m.y));
        const right = Math.max(...ms.map(m => m.x + m.width));
        const bottom = Math.max(...ms.map(m => m.y + m.height));
        const vertical = r.edge === 'left' || r.edge === 'right';
        const origin = vertical ? top : left;
        const span = vertical ? bottom - top : right - left;
        const coordinate = Math.min(origin + span - 1, origin + Math.floor(r.position * span / MAX));
        const boundary = {left, right, top, bottom}[r.edge];
        const segments = ms.filter(m => ({left: m.x, right: m.x + m.width, top: m.y, bottom: m.y + m.height}[r.edge]) === boundary)
            .map(m => ({
                start: Math.max(vertical ? m.y : m.x, origin + Math.ceil(r.start * span / MAX)),
                end: Math.min(vertical ? m.y + m.height : m.x + m.width, origin + Math.floor(r.end * span / MAX)),
                monitor: m,
            })).filter(s => s.end > s.start);
        const segment = segments.find(s => coordinate >= s.start && coordinate < s.end);
        if (!segment) throw new Error('The selected crossing enters a gap between GNOME monitors');
        const m = segment.monitor;
        if (m.width < 8 || m.height < 8) throw new Error('The entry monitor is too small');
        const point = vertical ? {x: r.edge === 'left' ? left + 3 : right - 4, y: coordinate}
            : {x: coordinate, y: r.edge === 'top' ? top + 3 : bottom - 4};
        const lease = {token: r.token, renewed: GLib.get_monotonic_time(), returned: null, polls: new Set()};
        this._lease = lease;
        try {
            Clutter.get_default_backend().get_default_seat().warp_pointer(point.x, point.y);
            const directions = {left: Meta.BarrierDirection.POSITIVE_X, right: Meta.BarrierDirection.NEGATIVE_X,
                top: Meta.BarrierDirection.POSITIVE_Y, bottom: Meta.BarrierDirection.NEGATIVE_Y};
            for (const s of segments) {
                const barrier = new Meta.Barrier({backend: global.backend, directions: directions[r.edge],
                    x1: vertical ? boundary : s.start, x2: vertical ? boundary : s.end,
                    y1: vertical ? s.start : boundary, y2: vertical ? s.end : boundary});
                barrier.connect('hit', (_barrier, event) => {
                    if (this._lease !== lease || lease.returned !== null) return;
                    const axis = vertical ? event.y : event.x;
                    lease.returned = Math.max(r.start, Math.min(r.end, Math.round((axis - origin) * MAX / span)));
                    for (const reply of lease.polls) reply();
                });
                this._barriers.push(barrier);
            }
            // The seat processes a warp asynchronously; inspect the compositor
            // on the next main-loop turn before accepting the entry.
            await new Promise(resolve => GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {resolve(); return GLib.SOURCE_REMOVE;}));
            if (this._lease !== lease) throw new Error('Desktop changed during entry');
            const actual = this._snapshot();
            if (Math.abs(actual.position.x - point.x) > 2 || Math.abs(actual.position.y - point.y) > 2)
                throw new Error('GNOME did not place the cursor at the requested entry');
            return {status: 'prepared', ...actual};
        } catch (error) {
            if (this._lease === lease) this._clear();
            throw error;
        }
    }
}
