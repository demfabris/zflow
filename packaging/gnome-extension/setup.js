import Adw from 'gi://Adw?version=1';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

// The "finish setup" banner of the zflow window. It is not part of the
// extension on extensions.gnome.org, because it installs and enables one.
const UUID = 'zflow@demfabris';
// GNOME Shell's ExtensionState values.
const ACTIVE = 1;
const INACTIVE = 2;
const ERROR = 3;
const OUT_OF_DATE = 4;
const INITIALIZED = 6;

const STEPS = {
    install: {title: 'Install the zflow GNOME extension to finish setup', button: 'Install'},
    enable: {title: 'The zflow GNOME extension is turned off', button: 'Turn On'},
    logout: {title: 'Log out and back in to finish setting up zflow', button: 'Log Out'},
    error: {title: 'The zflow GNOME extension stopped. Log out and back in to restart it', button: 'Log Out'},
    outdated: {title: 'The zflow GNOME extension does not support this GNOME version. Update zflow', button: null},
};

function call(bus, path, iface, method, parameters, cancellable) {
    return new Promise((resolve, reject) => {
        Gio.DBus.session.call(bus, path, iface, method, parameters, null,
            Gio.DBusCallFlags.NO_AUTO_START, 5000, cancellable, (connection, result) => {
                try { resolve(connection.call_finish(result).recursiveUnpack()); }
                catch (error) { reject(error); }
            });
    });
}

function shellCall(method, cancellable) {
    return call('org.gnome.Shell', '/org/gnome/Shell', 'org.gnome.Shell.Extensions', method,
        new GLib.Variant('(s)', [UUID]), cancellable);
}

// Makes GNOME turn the extension on when it loads it at the next login.
function enableAtLogin() {
    if (!Gio.SettingsSchemaSource.get_default().lookup('org.gnome.shell', true)) return;
    const shell = new Gio.Settings({schema_id: 'org.gnome.shell'});
    const enabled = shell.get_strv('enabled-extensions');
    if (!enabled.includes(UUID)) shell.set_strv('enabled-extensions', [...enabled, UUID]);
    const disabled = shell.get_strv('disabled-extensions');
    if (disabled.includes(UUID)) shell.set_strv('disabled-extensions', disabled.filter(uuid => uuid !== UUID));
    Gio.Settings.sync();
}

export class Setup {
    constructor(client) {
        this._client = client;
        this._cancel = new Gio.Cancellable();
        this._step = null;
        this._error = '';
        this.banner = new Adw.Banner({revealed: false});
        this.banner.connect('button-clicked', () => this._act());
        this._timer = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 2, () => {
            this.refresh();
            return GLib.SOURCE_CONTINUE;
        });
        this.refresh();
    }

    async refresh() {
        let step = null;
        try {
            const [info] = await shellCall('GetExtensionInfo', this._cancel);
            if (info.state === undefined) step = this._installed() ? 'logout' : 'install';
            else if (info.state === INACTIVE || info.state === INITIALIZED) step = 'enable';
            else if (info.state === ERROR) step = 'error';
            else if (info.state === OUT_OF_DATE) step = 'outdated';
        } catch {
            // Without GNOME Shell there is nothing to set up here.
        }
        if (this._cancel.is_cancelled()) return;
        if (step !== this._step) this._error = '';
        this._step = step;
        this._show();
    }

    // GNOME Shell looks in these folders when the session starts.
    _installed() {
        return [GLib.get_user_data_dir(), ...GLib.get_system_data_dirs()].some(dir => GLib.file_test(
            GLib.build_filenamev([dir, 'gnome-shell', 'extensions', UUID, 'metadata.json']), GLib.FileTest.EXISTS));
    }

    _show() {
        const step = STEPS[this._step];
        this.banner.revealed = !!step;
        if (!step) return;
        this.banner.title = this._error || step.title;
        this.banner.button_label = step.button;
    }

    async _act() {
        const step = this._step;
        this.banner.sensitive = false;
        try {
            if (step === 'install') {
                // GNOME asks before it downloads, so wait for the user as long as it takes.
                await this._client.call({command: 'install_extension'}, GLib.MAXINT32);
            } else if (step === 'enable') {
                const [enabled] = await shellCall('EnableExtension', this._cancel);
                if (!enabled) throw new Error('GNOME could not turn on the zflow extension');
            } else if (step === 'logout' || step === 'error') {
                enableAtLogin();
                // GNOME shows its own confirmation first.
                await call('org.gnome.SessionManager', '/org/gnome/SessionManager', 'org.gnome.SessionManager',
                    'Logout', new GLib.Variant('(u)', [0]), this._cancel);
            }
        } catch (error) {
            if (error instanceof GLib.Error) Gio.DBusError.strip_remote_error(error);
            this._error = error.message;
        } finally {
            if (!this._cancel.is_cancelled()) {
                this.banner.sensitive = true;
                this._show();
                this.refresh();
            }
        }
    }

    destroy() {
        if (this._timer) GLib.Source.remove(this._timer);
        this._timer = 0;
        this._cancel.cancel();
    }
}
