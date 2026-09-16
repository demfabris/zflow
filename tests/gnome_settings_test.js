// Run on a private bus: dbus-run-session -- gjs -m tests/gnome_settings_test.js
import Adw from 'gi://Adw?version=1';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import {Settings} from '../packaging/gnome-extension/settings.js';
import {statusText} from '../packaging/gnome-extension/client.js';

function assert(value, message) { if (!value) throw new Error(message); }
function waitFor(predicate) {
    return new Promise((resolve, reject) => {
        let attempts = 0;
        GLib.timeout_add(GLib.PRIORITY_DEFAULT, 20, () => {
            if (predicate()) { resolve(); return GLib.SOURCE_REMOVE; }
            if (++attempts > 350) { reject(new Error('Timed out waiting for GTK state')); return GLib.SOURCE_REMOVE; }
            return GLib.SOURCE_CONTINUE;
        });
    });
}

const snapshot = {
    daemon: {sharing: true, peers: {MacBook: {}}, connected: [], receiving_from: null, sending_to: null},
    desktop_ready: true, desktop: 'Ready', autostart: true, pairing: {state: 'idle'}, nearby: [],
};
let failSharing = false;
let callCount = 0;
const xml = '<node><interface name="io.zflow.Desktop"><method name="Call"><arg type="s" direction="in"/><arg type="s" direction="out"/></method></interface></node>';
const object = Gio.DBusExportedObject.wrapJSObject(xml, {
    Call(json) {
        const request = JSON.parse(json);
        callCount++;
        switch (request.command) {
        case 'snapshot': return JSON.stringify(snapshot);
        case 'set_sharing':
            if (failSharing) throw new Error('Test: service refused the change');
            snapshot.daemon.sharing = request.enabled;
            break;
        case 'set_autostart': snapshot.autostart = request.enabled; break;
        case 'forget': delete snapshot.daemon.peers[request.name]; break;
        case 'pair': snapshot.pairing = {state: 'confirm', name: 'Mac mini', code: '123456'}; break;
        case 'pair_confirm':
            assert(request.code === '123456', 'confirmation must forward the entered code');
            snapshot.pairing = {state: 'paired'};
            snapshot.daemon.peers[request.name] = {};
            break;
        case 'pair_cancel': snapshot.pairing = {state: 'idle'}; break;
        default: throw new Error(`Unexpected request ${request.command}`);
        }
        return JSON.stringify({ok: true});
    },
});
object.export(Gio.DBus.session, '/io/zflow/Desktop');
let owned = false;
const owner = Gio.bus_own_name_on_connection(Gio.DBus.session, 'io.zflow.Desktop', Gio.BusNameOwnerFlags.NONE, () => { owned = true; }, null);
let failure = null;
const app = new Adw.Application({application_id: 'io.zflow.SettingsTest'});
app.connect('activate', () => {
    const window = new Adw.ApplicationWindow({application: app, default_width: 520, default_height: 640});
    const settings = new Settings(window);
    window.content = settings.page;
    window.present();
    (async () => {
        await waitFor(() => owned && settings._sharing.sensitive);
        assert(settings._status.title === 'Ready to share', 'ready status');
        const firstRow = settings._peerRows[0];
        await settings.client.refresh();
        assert(settings._peerRows[0] === firstRow, 'refresh must preserve keyboard focus');
        settings._sharing.active = false;
        await waitFor(() => !snapshot.daemon.sharing && !settings._busy);
        assert(settings._status.title === 'Sharing paused', 'pause reaches daemon and refreshes status');
        failSharing = true;
        settings._sharing.active = true;
        await waitFor(() => !settings._busy && settings._errorGroup.visible);
        assert(!settings._sharing.active, 'rejected toggle rolls back');
        failSharing = false;
        settings._login.active = false;
        await waitFor(() => !snapshot.autostart && !settings._busy);
        settings._openPairing();
        await settings._startPair(null);
        await waitFor(() => settings._pairing.confirmation.visible);
        settings._pairing.entered.text = '123456';
        settings._pairing.confirm.emit('clicked');
        await waitFor(() => snapshot.pairing.state === 'paired' && !settings._busy);
        assert(snapshot.daemon.peers['Mac mini'] !== undefined, 'pairing saves confirmed name');
        settings._pairing.dialog.close();
        await waitFor(() => settings._pairing === null && snapshot.pairing.state === 'idle');
        settings._openPairing();
        await settings._startPair(null);
        settings._pairing.dialog.close();
        await waitFor(() => snapshot.pairing.state === 'idle');
        await settings._run({command: 'forget', name: 'MacBook'});
        assert(!snapshot.daemon.peers.MacBook, 'forget uses daemon API');
        snapshot.daemon = null;
        snapshot.error = 'Start the zflow system service';
        await settings.client.refresh();
        assert(!settings._sharing.sensitive && !settings._pairButton.sensitive, 'offline controls disabled');
        assert(statusText(snapshot) === 'Service unavailable', 'offline status');
        settings.destroy();
        await waitFor(() => !settings.client._polling);
        const before = callCount;
        await settings.client.refresh();
        assert(callCount === before, 'closed window stops polling');
        print('GTK settings: status, focus, pause, rollback, login, pairing, cancel, forget, offline and cleanup passed');
    })().catch(error => { failure = error; printerr(error.stack); }).finally(() => {
        settings.destroy();
        object.unexport();
        Gio.bus_unown_name(owner);
        app.quit();
    });
});
app.run([]);
if (failure) throw failure;
