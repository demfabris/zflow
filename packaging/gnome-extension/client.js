import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

const BUS = 'io.zflow.Desktop';
const PATH = '/io/zflow/Desktop';
// src/app/gnome.rs mirrors this. The extension comes from extensions.gnome.org
// and the agent from the zflow package, so they can be updated at different times.
export const API = 5;

// Agents older than API 1 did not report it and speak API 1.
export function compatible(snapshot) {
    return (snapshot?.api ?? 1) === API;
}

export function statusText(snapshot) {
    if (snapshot?.agent_error) return 'zflow is not running';
    if (!compatible(snapshot)) return 'Update zflow';
    return snapshot.status.title;
}

// Whether something has to be fixed before input can move.
export function needsAttention(snapshot) {
    return !compatible(snapshot) || snapshot.health.some(row => row.level === 'error');
}

export class Client {
    constructor(changed, activate = false) {
        this._activate = activate;
        this._changed = changed;
        this._cancel = new Gio.Cancellable();
        this._timer = 0;
        this._polling = false;
        this.snapshot = null;
    }

    call(request, timeout = 7000) {
        const flags = request.command === 'snapshot' && !this._activate ? Gio.DBusCallFlags.NO_AUTO_START : Gio.DBusCallFlags.NONE;
        if (request.command === 'snapshot') this._activate = false;
        return new Promise((resolve, reject) => {
            Gio.DBus.session.call(BUS, PATH, BUS, 'Call',
                new GLib.Variant('(s)', [JSON.stringify(request)]),
                new GLib.VariantType('(s)'), flags, timeout, this._cancel,
                (connection, result) => {
                    try { resolve(JSON.parse(connection.call_finish(result).deep_unpack()[0])); }
                    catch (error) {
                        // Users see this message, so drop the "GDBus.Error:<name>: " prefix.
                        if (error instanceof GLib.Error) Gio.DBusError.strip_remote_error(error);
                        reject(error);
                    }
                });
        });
    }

    start() {
        this.refresh();
        this._timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 1000, () => {
            this.refresh();
            return GLib.SOURCE_CONTINUE;
        });
    }

    async refresh() {
        if (this._polling || this._cancel.is_cancelled()) return;
        this._polling = true;
        try {
            this.snapshot = await this.call({command: 'snapshot'});
        } catch (error) {
            this.snapshot = {agent_error: true, error: `Open zflow settings to start the desktop agent. ${error.message}`};
        } finally {
            this._polling = false;
        }
        if (!this._cancel.is_cancelled()) this._changed(this.snapshot);
    }

    destroy() {
        if (this._timer) GLib.Source.remove(this._timer);
        this._timer = 0;
        this._cancel.cancel();
    }
}
