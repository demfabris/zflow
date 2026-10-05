// GTK_A11Y=none GIO_USE_VFS=local dbus-run-session --config-file=tests/gtk-session.conf -- gjs -m tests/gnome_updates_test.js
import Adw from 'gi://Adw?version=1';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Gtk from 'gi://Gtk?version=4.0';
import {Updates} from '../packaging/gnome-extension/updates.js';

function assert(value, message) { if (!value) throw new Error(message); }
function waitFor(predicate) {
    return new Promise((resolve, reject) => {
        let attempts = 0;
        GLib.timeout_add(GLib.PRIORITY_DEFAULT, 20, () => {
            if (predicate()) { resolve(); return GLib.SOURCE_REMOVE; }
            if (++attempts > 250) { reject(new Error(`Timed out waiting for updates: ${predicate}`)); return GLib.SOURCE_REMOVE; }
            return GLib.SOURCE_CONTINUE;
        });
    });
}
function responseButton(widget, label) {
    if (widget instanceof Gtk.Button && widget.label === label) return widget;
    for (let child = widget.get_first_child(); child; child = child.get_next_sibling()) {
        const found = responseButton(child, label);
        if (found) return found;
    }
    return null;
}
const directory = GLib.dir_make_tmp('zflow-update-test-XXXXXX');
GLib.setenv('XDG_CONFIG_HOME', directory, true);
assert(GLib.get_user_config_dir() === directory, 'preferences stay in the test directory');
GLib.setenv('ZFLOW_UPDATE_TEST_DIR', directory, true);
GLib.setenv('ZFLOW_VERSION', '0.5.0', true);
const executable = `${directory}/fixture-cli`;
GLib.setenv('ZFLOW_EXECUTABLE', executable, true);
GLib.file_set_contents(executable, `#!/bin/sh
printf '%s\\n' "$*" >> "$ZFLOW_UPDATE_TEST_DIR/calls"
case "$1 $2" in
    'update check')
        cat "$ZFLOW_UPDATE_TEST_DIR/result"
        cat "$ZFLOW_UPDATE_TEST_DIR/error" >&2
        exit "$(cat "$ZFLOW_UPDATE_TEST_DIR/status")" ;;
    'update install')
        cat "$ZFLOW_UPDATE_TEST_DIR/install-error" >&2
        exit "$(cat "$ZFLOW_UPDATE_TEST_DIR/install-status")" ;;
    'settings ')
        printf '%s' "$ZFLOW_REPLACE" > "$ZFLOW_UPDATE_TEST_DIR/replacement" ;;
    *) exit 99 ;;
esac
`);
Gio.File.new_for_path(executable).set_attribute_uint32('unix::mode', 0o700, Gio.FileQueryInfoFlags.NONE, null);
const release = {current: '0.5.0', latest: 'v0.6.0', available: true, can_install: true, reason: null};
function setResult(value = release, error = '', status = 0) {
    GLib.file_set_contents(`${directory}/result`, JSON.stringify(value));
    GLib.file_set_contents(`${directory}/error`, error);
    GLib.file_set_contents(`${directory}/status`, String(status));
}
function setInstall(error = '', status = 0) {
    GLib.file_set_contents(`${directory}/install-error`, error);
    GLib.file_set_contents(`${directory}/install-status`, String(status));
}
function calls() {
    try { return new TextDecoder().decode(GLib.file_get_contents(`${directory}/calls`)[1]).trim().split('\n'); }
    catch { return []; }
}
setResult();
setInstall();
let failure = null;
const app = new Adw.Application({application_id: 'io.zflow.UpdatesTest'});
app.connect('activate', () => {
    const window = new Adw.ApplicationWindow({application: app, default_width: 520, default_height: 400});
    const updates = new Updates(window);
    const page = new Adw.PreferencesPage();
    page.add(updates.group);
    window.content = page;
    window.present();
    let restarted = 0;
    const restart = updates._restart.bind(updates);
    updates._restart = () => { restarted++; };
    (async () => {
        // If the host is metered, the explicit check still works.
        await waitFor(() => !updates._busy);
        if (!updates._release) await updates.check();
        assert(updates._button.label === 'Install…', 'a newer release offers installation');
        assert(calls().every(call => call === 'update check'), 'automatic checking never installs');

        updates._button.emit('clicked');
        await waitFor(() => window.visible_dialog);
        assert(window.visible_dialog instanceof Adw.AlertDialog, 'installation asks in a native dialog');
        assert(calls().every(call => call === 'update check'), 'opening confirmation never installs');
        // Activate the actual response button so libadwaita owns response and closing.
        const cancel = responseButton(window.visible_dialog, 'Cancel');
        await waitFor(() => cancel.get_mapped());
        assert(cancel.activate(), 'the native Cancel button activates');
        await waitFor(() => !window.visible_dialog);
        assert(!updates.installing && calls().every(call => call === 'update check'), 'Cancel leaves the system alone');

        setInstall('Error: Update cancelled. Nothing was installed.', 1);
        updates._button.emit('clicked');
        await waitFor(() => window.visible_dialog);
        const install = responseButton(window.visible_dialog, 'Install and Restart');
        await waitFor(() => install.get_mapped());
        assert(install.activate(), 'the native install button activates');
        await waitFor(() => !updates.installing && calls().some(call => call.startsWith('update install')));
        assert(calls().includes('update install --version v0.6.0 --gui'), 'approval pins the displayed release and requires graphical authorization');
        assert(updates._row.subtitle.startsWith('Update cancelled.'), 'cancelled system authorization is shown');
        assert(restarted === 0 && updates._button.sensitive, 'cancellation allows another attempt without restarting');

        setResult(release, 'Error: Could not check for updates.', 1);
        await updates.check();
        assert(updates._button.label === 'Retry' && updates._row.subtitle === 'Could not check for updates.', 'offline checks show a retry action');
        setResult({...release, available: false, latest: 'v0.5.0'});
        await updates.check();
        assert(updates._button.label === 'Check' && updates._row.subtitle === 'You have the latest version.', 'an equal or older release cannot be installed');
        setResult({...release, can_install: false, reason: 'Update zflow through pacman, which manages this installation.'});
        await updates.check();
        assert(updates._button.label === 'Check' && updates._row.subtitle.includes('pacman'), 'package ownership controls are visible');

        updates._automatic.active = false;
        assert(!updates._loadAutomatic(), 'the automatic-check preference is saved');
        const before = calls().length;
        const another = new Updates(window);
        assert(!another._automatic.active && !another._busy && calls().length === before, 'opening settings honors disabled checks');
        another.destroy();

        setResult({...release, current: '0.6.0', available: false});
        await updates.check();
        assert(updates._button.label === 'Restart', 'an externally updated binary asks to restart the older window');
        updates._button.emit('clicked');
        assert(restarted === 1, 'Restart reloads the installed version without another installation');
        updates._restartNeeded = false;
        setResult();
        setInstall();
        await updates.check();
        await updates._install('v0.6.0');
        assert(restarted === 2 && updates._restartNeeded, 'a successful installation restarts settings');
        const quit = app.quit.bind(app);
        let quitRequested = false;
        app.quit = () => { quitRequested = true; };
        restart();
        app.quit = quit;
        await waitFor(() => GLib.file_test(`${directory}/replacement`, GLib.FileTest.EXISTS));
        assert(quitRequested && new TextDecoder().decode(GLib.file_get_contents(`${directory}/replacement`)[1]) === '1',
            'restart launches the installed settings with native application replacement before quitting');
        updates.destroy();
        assert(updates._timer === 0 && updates._cancel.is_cancelled(), 'closing settings stops checks');
        print('GTK updates: checks, consent, cancellation, offline retry, package ownership, preferences and restart passed');
    })().catch(error => { failure = error; printerr(error.stack); }).finally(() => {
        updates.destroy();
        app.quit();
    });
});
try { app.run([]); }
finally {
    // The only generated files live in this fresh directory.
    for (const path of ['zflow/updates.json', 'zflow', 'fixture-cli', 'result', 'error', 'status', 'install-error', 'install-status', 'calls', 'replacement']) {
        try { Gio.File.new_for_path(`${directory}/${path}`).delete(null); } catch { /* May not have been created. */ }
    }
    try { Gio.File.new_for_path(directory).delete(null); } catch { /* GIO may have created a cache. */ }
}
if (failure) throw failure;
