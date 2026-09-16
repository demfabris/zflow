import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

const BUS = 'io.zflow.Desktop';
const PATH = '/io/zflow/Desktop';

export function statusText(snapshot) {
    if (snapshot?.agent_error) return 'zflow is not running';
    if (!snapshot?.daemon) return 'Service unavailable';
    const daemon = snapshot.daemon;
    if (!daemon.sharing) return 'Sharing paused';
    if (!snapshot.desktop_ready) return 'Desktop needs attention';
    if (daemon.receiving_from) return `Receiving from ${daemon.receiving_from}`;
    if (daemon.sending_to) return `Controlling ${daemon.sending_to}`;
    return Object.keys(daemon.peers).length ? 'Ready to share' : 'Pair a computer to get started';
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

    call(request) {
        const flags = request.command === 'snapshot' && !this._activate ? Gio.DBusCallFlags.NO_AUTO_START : Gio.DBusCallFlags.NONE;
        if (request.command === 'snapshot') this._activate = false;
        return new Promise((resolve, reject) => {
            Gio.DBus.session.call(BUS, PATH, BUS, 'Call',
                new GLib.Variant('(s)', [JSON.stringify(request)]),
                new GLib.VariantType('(s)'), flags, 7000, this._cancel,
                (connection, result) => {
                    try { resolve(JSON.parse(connection.call_finish(result).deep_unpack()[0])); }
                    catch (error) { reject(error); }
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
