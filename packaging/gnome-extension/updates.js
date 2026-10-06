import Adw from 'gi://Adw?version=1';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Gtk from 'gi://Gtk?version=4.0';

// Only the installed app imports this file. The GNOME extension has its own
// update mechanism through extensions.gnome.org.
const INTERVAL = 6 * 60 * 60;

export class Updates {
    constructor(window) {
        this._window = window;
        this._executable = GLib.getenv('ZFLOW_EXECUTABLE');
        this._version = GLib.getenv('ZFLOW_VERSION') ?? 'unknown';
        this._file = Gio.File.new_for_path(GLib.build_filenamev([GLib.get_user_config_dir(), 'zflow', 'updates.json']));
        this._cancel = new Gio.Cancellable();
        this._busy = false;
        this.installing = false;
        this._release = null;
        this._restartNeeded = false;
        this._disposed = false;
        this.group = new Adw.PreferencesGroup({title: 'Updates'});
        this._row = new Adw.ActionRow({title: `zflow ${this._version}`, subtitle: 'Check for a new release.', subtitle_lines: 0, use_markup: false});
        this._button = new Gtk.Button({label: 'Check', valign: Gtk.Align.CENTER});
        this._button.connect('clicked', () => this._act());
        this._row.add_suffix(this._button);
        this.group.add(this._row);
        this._automatic = new Adw.SwitchRow({title: 'Check Automatically', subtitle: 'Check when you open zflow and every six hours while it is open.'});
        this._automatic.active = this._loadAutomatic();
        this._automatic.connect('notify::active', () => {
            try {
                this._file.get_parent().make_directory_with_parents(null);
            } catch (error) {
                if (!error.matches(Gio.io_error_quark(), Gio.IOErrorEnum.EXISTS)) {
                    this._row.subtitle = error.message;
                    return;
                }
            }
            try {
                this._file.replace_contents(JSON.stringify({automatic: this._automatic.active}), null, false, Gio.FileCreateFlags.PRIVATE, null);
                if (this._automatic.active) this.check(true);
            } catch (error) { this._row.subtitle = error.message; }
        });
        this.group.add(this._automatic);
        this._timer = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, INTERVAL, () => {
            if (this._automatic.active) this.check(true);
            return GLib.SOURCE_CONTINUE;
        });
        this.opened();
    }

    // Opening zflow checks, also when its window is already open, such as
    // from the notification the desktop agent shows for a new release.
    opened() {
        if (this._automatic.active) this.check(true);
    }

    _loadAutomatic() {
        try {
            const [, bytes] = this._file.load_contents(null);
            return JSON.parse(new TextDecoder().decode(bytes)).automatic !== false;
        } catch { return true; }
    }

    _run(args, cancellable = this._cancel) {
        return new Promise((resolve, reject) => {
            if (!this._executable) { reject(new Error('Open zflow from Applications to check for updates.')); return; }
            let process;
            try {
                process = Gio.Subprocess.new([this._executable, 'update', ...args], Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE);
            } catch (error) { reject(error); return; }
            process.communicate_utf8_async(null, cancellable, (child, result) => {
                try {
                    const [, stdout, stderr] = child.communicate_utf8_finish(result);
                    if (!child.get_successful()) throw new Error((stderr.trim() || stdout.trim() || 'The update did not finish. Please try again.').replace(/^Error: /, ''));
                    resolve(stdout);
                } catch (error) { reject(error); }
            });
        });
    }

    async check(automatic = false) {
        if (this._busy || this._disposed || this._restartNeeded) return;
        if (automatic && Gio.NetworkMonitor.get_default().network_metered) {
            this._row.subtitle = 'Automatic checks are paused on a metered connection. You can still check manually.';
            return;
        }
        this._busy = true;
        this._button.sensitive = false;
        this._row.subtitle = 'Checking for updates…';
        try {
            const result = JSON.parse(await this._run(['check']));
            if (this._disposed) return;
            this._release = result;
            if (result.current !== this._version) {
                this._restartNeeded = true;
                this._row.subtitle = `zflow ${result.current} is installed. Restart to use it.`;
                this._button.label = 'Restart';
            } else if (result.available) {
                this._row.subtitle = result.can_install ? `${result.latest} is available.` : `${result.latest} is available. ${result.reason}`;
                this._button.label = result.can_install ? 'Install…' : 'Check';
            } else {
                this._row.subtitle = result.can_install ? 'You have the latest version.' : result.reason;
                this._button.label = 'Check';
            }
        } catch (error) {
            this._release = null;
            if (!this._disposed) { this._row.subtitle = error.message; this._button.label = 'Retry'; }
        } finally {
            this._busy = false;
            if (!this._disposed) this._button.sensitive = true;
        }
    }

    _act() {
        if (this._busy || this._disposed) return;
        if (this._restartNeeded) { this._restart(); return; }
        if (!this._release?.available || !this._release.can_install) { this.check(); return; }
        const release = this._release.latest;
        const dialog = new Adw.AlertDialog({
            heading: `Install zflow ${release}?`,
            body: 'Input sharing will disconnect briefly. zflow will ask for administrator approval, install the update, and restart this window. Your settings and paired computers will stay.',
            close_response: 'cancel', default_response: 'cancel',
        });
        dialog.add_response('cancel', 'Cancel');
        dialog.add_response('install', 'Install and Restart');
        dialog.set_response_appearance('install', Adw.ResponseAppearance.SUGGESTED);
        dialog.connect('response', (_dialog, response) => {
            if (response === 'install') this._install(release);
        });
        dialog.present(this._window);
    }

    async _install(release) {
        if (this._busy || this._disposed) return;
        this._busy = this.installing = true;
        this._button.sensitive = this._automatic.sensitive = false;
        this._row.subtitle = 'Downloading and installing… Approve the system prompt to continue.';
        try {
            // Do not cancel an authorized package transaction when a window closes.
            await this._run(['install', '--version', release, '--gui'], null);
            this._restartNeeded = true;
            this._button.label = 'Restart';
            this._row.subtitle = `${release} is installed. Restart to use it.`;
            this._restart();
        } catch (error) { this._row.subtitle = error.message; }
        finally {
            this._busy = this.installing = false;
            if (!this._disposed) this._button.sensitive = this._automatic.sensitive = true;
        }
    }

    _restart() {
        try {
            const launcher = new Gio.SubprocessLauncher({flags: Gio.SubprocessFlags.NONE});
            launcher.setenv('ZFLOW_REPLACE', '1', true);
            launcher.spawnv([this._executable, 'settings']);
            this._window.application.quit();
        } catch (error) { this._row.subtitle = `The update is installed, but zflow could not restart. ${error.message}`; }
    }

    destroy() {
        this._disposed = true;
        if (this._timer) GLib.Source.remove(this._timer);
        this._timer = 0;
        this._cancel.cancel();
    }
}
