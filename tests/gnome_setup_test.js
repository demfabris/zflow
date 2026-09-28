// Run on a private bus with in-memory settings:
// GSETTINGS_BACKEND=memory dbus-run-session -- gjs -m tests/gnome_setup_test.js
import Adw from 'gi://Adw?version=1';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import {Client} from '../packaging/gnome-extension/client.js';
import {Setup} from '../packaging/gnome-extension/setup.js';

function assert(value, message) { if (!value) throw new Error(message); }
function waitFor(predicate) {
    return new Promise((resolve, reject) => {
        let attempts = 0;
        GLib.timeout_add(GLib.PRIORITY_DEFAULT, 20, () => {
            if (predicate()) { resolve(); return GLib.SOURCE_REMOVE; }
            if (++attempts > 350) { reject(new Error('Timed out waiting for the banner')); return GLib.SOURCE_REMOVE; }
            return GLib.SOURCE_CONTINUE;
        });
    });
}

// The Log Out step writes GNOME settings; never let that reach the real account.
assert(GLib.getenv('GSETTINGS_BACKEND') === 'memory', 'run with GSETTINGS_BACKEND=memory');

const shell = {state: undefined, calls: []};
const agentCalls = [];
let installError = null;
const bus = Gio.DBus.session;
function serve(name, path, xml, implementation) {
    const object = Gio.DBusExportedObject.wrapJSObject(xml, implementation);
    object.export(bus, path);
    let owned = false;
    Gio.bus_own_name_on_connection(bus, name, Gio.BusNameOwnerFlags.NONE, () => { owned = true; }, null);
    return () => owned;
}
const shellOwned = serve('org.gnome.Shell', '/org/gnome/Shell',
    `<node><interface name="org.gnome.Shell.Extensions">
        <method name="GetExtensionInfo"><arg type="s" direction="in"/><arg type="a{sv}" direction="out"/></method>
        <method name="EnableExtension"><arg type="s" direction="in"/><arg type="b" direction="out"/></method>
    </interface></node>`, {
        GetExtensionInfo(uuid) {
            assert(uuid === 'zflow@demfabris', 'the banner asks about zflow');
            return shell.state === undefined ? {} : {state: new GLib.Variant('d', shell.state)};
        },
        EnableExtension(uuid) {
            shell.calls.push(`enable ${uuid}`);
            shell.state = 1;
            return true;
        },
    });
const agentOwned = serve('io.zflow.Desktop', '/io/zflow/Desktop',
    '<node><interface name="io.zflow.Desktop"><method name="Call"><arg type="s" direction="in"/><arg type="s" direction="out"/></method></interface></node>', {
        Call(json) {
            const request = JSON.parse(json);
            agentCalls.push(request.command);
            if (installError) throw new Error(installError);
            shell.state = 1;
            return JSON.stringify({ok: true, running: true});
        },
    });
const sessionOwned = serve('org.gnome.SessionManager', '/org/gnome/SessionManager',
    '<node><interface name="org.gnome.SessionManager"><method name="Logout"><arg type="u" direction="in"/></method></interface></node>', {
        Logout(mode) { shell.calls.push(`logout ${mode}`); },
    });

let failure = null;
const app = new Adw.Application({application_id: 'io.zflow.SetupTest'});
app.connect('activate', () => {
    const window = new Adw.ApplicationWindow({application: app, default_width: 520, default_height: 640});
    const toolbar = new Adw.ToolbarView();
    const client = new Client(() => {});
    const setup = new Setup(client);
    // Whether the extension's files are on disk, without reading this account's folders.
    let files = false;
    setup._installed = () => files;
    toolbar.add_top_bar(setup.banner);
    window.content = toolbar;
    window.present();
    const banner = setup.banner;
    (async () => {
        await waitFor(() => shellOwned() && agentOwned() && sessionOwned());
        await setup.refresh();
        assert(banner.revealed && banner.button_label === 'Install', 'an unknown extension without files offers Install');
        banner.emit('button-clicked');
        await waitFor(() => !banner.revealed);
        assert(agentCalls.join() === 'install_extension', 'Install asks the agent, which tries extensions.gnome.org first');

        shell.state = 6;
        await setup.refresh();
        assert(banner.button_label === 'Turn On', 'a loaded extension that is off offers Turn On');
        banner.emit('button-clicked');
        await waitFor(() => !banner.revealed);
        assert(shell.calls.join() === 'enable zflow@demfabris', 'Turn On uses GNOME Shell');

        shell.state = undefined;
        files = true;
        await setup.refresh();
        assert(banner.button_label === 'Log Out', 'files GNOME has not loaded yet need a new login');
        banner.emit('button-clicked');
        await waitFor(() => shell.calls.includes('logout 0'));
        const settings = new Gio.Settings({schema_id: 'org.gnome.shell'});
        assert(settings.get_strv('enabled-extensions').includes('zflow@demfabris'), 'Log Out turns the extension on for the next login');

        shell.state = 4;
        await setup.refresh();
        assert(banner.revealed && !banner.button_label, 'nothing to click for an unsupported GNOME version');

        files = false;
        shell.state = undefined;
        installError = 'Test: gsettings is missing';
        await setup.refresh();
        banner.emit('button-clicked');
        await waitFor(() => banner.title === installError);
        installError = null;

        shell.state = 1;
        await setup.refresh();
        assert(!banner.revealed, 'a running extension hides the banner');
        print('GTK setup banner: install, turn on, log out, unsupported GNOME, errors and hiding passed');
    })().catch(error => { failure = error; printerr(error.stack); }).finally(() => {
        setup.destroy();
        client.destroy();
        app.quit();
    });
});
app.run([]);
if (failure) throw failure;
