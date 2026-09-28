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
    daemon: {sharing: true, peers: {MacBook: {permissions: {connect: true, send_normal: true, receive_normal: true, inject_prelogin: false}}}, connected: [], receiving_from: null, sending_to: null},
    desktop_ready: true, desktop: 'Ready', autostart: true, pairing: {state: 'idle'}, nearby: [],
};
let failSharing = false;
let callCount = 0;
let keyboardCalls = 0;
let controlCalls = 0;
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
        case 'set_peer': {
            const peer = snapshot.daemon.peers[request.name];
            assert(('keyboard' in request) !== ('allow_control' in request), 'each control changes one field');
            if ('keyboard' in request) {
                keyboardCalls++;
                // The daemon leaves the default out of the peer record.
                if (request.keyboard === 'standard') delete peer.keyboard;
                else peer.keyboard = request.keyboard;
            } else {
                controlCalls++;
                peer.permissions.send_normal = request.allow_control;
            }
            break;
        }
        case 'pair':
            if (request.remote === null) {
                snapshot.pairing = {state: 'listening', code: '482 913'};
            } else {
                assert(request.remote === '192.0.2.7' && request.code === '123456', 'connecting forwards the address and the typed code');
                snapshot.pairing = {state: 'paired', name: 'Mac mini'};
                snapshot.daemon.peers['Mac mini'] = {};
            }
            break;
        case 'pair_respond':
            assert(snapshot.pairing.state === 'confirm', 'answers only a waiting question');
            if (request.allow) {
                snapshot.pairing = {state: 'paired', name: snapshot.pairing.name};
                snapshot.daemon.peers[snapshot.pairing.name] = {};
            } else {
                snapshot.pairing = {state: 'failed', error: 'Pairing declined'};
            }
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
        const keyboard = settings._keyboards.get('MacBook');
        assert(keyboard.selected === 0, 'a peer without a keyboard mode shows standard keys');
        keyboard.selected = 2;
        assert(!keyboard.sensitive, 'a request in flight locks the dropdown instead of dropping a choice');
        // Times out if the dropdown stays locked after the request.
        await waitFor(() => snapshot.daemon.peers.MacBook.keyboard === 'mac' && !settings._busy && keyboard.sensitive);
        keyboard.selected = 0;
        await waitFor(() => snapshot.daemon.peers.MacBook.keyboard === undefined && !settings._busy);
        assert(keyboardCalls === 2, 'each choice sends set_peer once');
        snapshot.daemon.peers.MacBook.keyboard = 'pc_positions';
        await waitFor(() => keyboard.selected === 1);
        assert(keyboardCalls === 2, 'a snapshot moves the dropdown without sending a request');
        assert(settings._peerRows[0] === firstRow && settings._keyboards.get('MacBook') === keyboard, 'a keyboard change keeps the row');
        const control = settings._controls.get('MacBook');
        assert(control.active, 'a paired computer can control this one');
        control.active = false;
        assert(!control.sensitive, 'a request in flight locks the switch');
        await waitFor(() => !snapshot.daemon.peers.MacBook.permissions.send_normal && !settings._busy && control.sensitive);
        snapshot.daemon.peers.MacBook.permissions.send_normal = true;
        await waitFor(() => control.active);
        assert(controlCalls === 1, 'a snapshot moves the switch without sending a request');
        assert(settings._peerRows[0] === firstRow, 'a permission change keeps the row');
        firstRow.expanded = true;
        snapshot.daemon.connected = ['MacBook'];
        snapshot.daemon.sending_to = 'MacBook';
        await waitFor(() => settings._peerRows[0] !== firstRow);
        assert(settings._peerRows[0].expanded, 'a rebuilt row stays open');
        assert(settings._peerRows[0].subtitle === 'Controlled from here', 'the row says this computer controls it');
        snapshot.daemon.sending_to = null;
        settings._sharing.active = false;
        await waitFor(() => !snapshot.daemon.sharing && !settings._busy);
        assert(settings._status.title === 'Sharing paused', 'pause reaches daemon and refreshes status');
        failSharing = true;
        settings._sharing.active = true;
        await waitFor(() => !settings._busy && settings._errorGroup.visible);
        assert(!settings._sharing.active, 'rejected toggle rolls back');
        assert(settings._error.subtitle === 'Test: service refused the change', 'service errors hide the D-Bus error name');
        failSharing = false;
        settings._login.active = false;
        await waitFor(() => !snapshot.autostart && !settings._busy);
        assert(!settings._pairing, 'a computer with peers does not open pairing by itself');
        settings._openPairing();
        await waitFor(() => settings._pairing.code.label === '482 913' && !settings._busy);
        settings._pairing.remote.text = '192.0.2.7';
        settings._pairing.entered.text = '123 456';
        settings._pairing.connect.emit('clicked');
        await waitFor(() => snapshot.pairing.state === 'paired' && !settings._busy);
        assert(snapshot.daemon.peers['Mac mini'] !== undefined, 'pairing saves the other computer');
        await waitFor(() => settings._pairing.stage.title === 'Paired with Mac mini');
        settings._pairing.dialog.close();
        await waitFor(() => settings._pairing === null && snapshot.pairing.state === 'idle');
        await settings._run({command: 'forget', name: 'MacBook'});
        await settings._run({command: 'forget', name: 'Mac mini'});
        assert(!snapshot.daemon.peers.MacBook, 'forget uses daemon API');
        const freshWindow = new Adw.ApplicationWindow({application: app, default_width: 520, default_height: 640});
        const fresh = new Settings(freshWindow);
        freshWindow.content = fresh.page;
        freshWindow.present();
        await waitFor(() => fresh._pairing !== null && snapshot.pairing.state === 'listening' && fresh._pairing.code.label === '482 913');
        // A computer proves the code; nothing is saved until the person here allows it.
        snapshot.pairing = {state: 'confirm', name: 'Stranger', address: '192.0.2.66'};
        await fresh.client.refresh();
        await waitFor(() => fresh._pairing.ask.visible && !fresh._pairing.shown.visible);
        assert(fresh._pairing.question.title === 'Allow Stranger to pair with this computer?', 'the question names the computer');
        assert(fresh._pairing.question.subtitle.includes('192.0.2.66'), 'the question shows its address');
        await fresh._respond(false);
        await waitFor(() => snapshot.pairing.state === 'failed' && fresh._pairing.renew.visible && !fresh._busy);
        assert(snapshot.daemon.peers.Stranger === undefined, 'declining saves nothing');
        await fresh._listen();
        await waitFor(() => snapshot.pairing.state === 'listening' && !fresh._busy);
        snapshot.pairing = {state: 'confirm', name: 'MacBook', address: '192.0.2.9'};
        await fresh.client.refresh();
        await waitFor(() => fresh._pairing.ask.visible);
        fresh._pairing.allow.emit('clicked');
        await waitFor(() => snapshot.pairing.state === 'paired' && !fresh._busy);
        assert(snapshot.daemon.peers.MacBook !== undefined, 'allowing saves the computer');
        await waitFor(() => fresh._pairing.stage.title === 'Paired with MacBook');
        delete snapshot.daemon.peers.MacBook;
        fresh._pairing.dialog.close();
        await waitFor(() => snapshot.pairing.state === 'idle');
        fresh.destroy();
        freshWindow.close();
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
        print('GTK settings: status, focus, keyboard mode, control permission, open rows, pause, rollback, login, pairing, first-run pairing, allow and decline, forget, offline and cleanup passed');
    })().catch(error => { failure = error; printerr(error.stack); }).finally(() => {
        settings.destroy();
        object.unexport();
        Gio.bus_unown_name(owner);
        app.quit();
    });
});
app.run([]);
if (failure) throw failure;
