import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const timers = new Map();
const calls = [];
let snapshot = {daemon: {sharing: true, peers: {Mac: {}}, connected: [], receiving_from: 'Mac'}, desktop_ready: true};
const context = {
    GLib: {
        Variant: class { constructor(_type, value) { this.value = value; } },
        VariantType: class {}, Error: class extends Error {}, PRIORITY_DEFAULT: 0, SOURCE_CONTINUE: true,
        timeout_add(_priority, _interval, callback) { const id = timers.size + 1; timers.set(id, callback); return id; },
        Source: {remove(id) { timers.delete(id); }},
    },
    Gio: {
        DBusCallFlags: {NONE: 0, NO_AUTO_START: 1},
        Cancellable: class { cancel() { this.cancelled = true; } is_cancelled() { return !!this.cancelled; } },
        DBus: {session: {
            call(_bus, _path, _interface, _method, parameters, _type, flags, _timeout, cancel, callback) {
                const request = JSON.parse(parameters.value[0]);
                calls.push({request, flags});
                if (request.command === 'set_sharing') snapshot.daemon.sharing = request.enabled;
                queueMicrotask(() => callback({call_finish() {
                    if (cancel.is_cancelled()) throw new Error('cancelled');
                    return {deep_unpack: () => [JSON.stringify(request.command === 'snapshot' ? snapshot : {ok: true})]};
                }}, {}));
            },
        }},
    },
    St: {Icon: class { constructor(props) {Object.assign(this, props);} }},
    PanelMenu: {Button: class {
        menu = {addMenuItem() {}, addAction() {}};
        handlers = {};
        add_child() {}
        connect(signal, callback) {this.handlers[signal] = callback;}
        destroy() {this.destroyed = (this.destroyed ?? 0) + 1; this.handlers.destroy?.();}
    }},
    PopupMenu: {
        PopupMenuItem: class {label = {};}, PopupSeparatorMenuItem: class {},
        PopupSwitchMenuItem: class {
            setSensitive(value) {this.sensitive = value;}
            setToggleState(value) {this.state = value;}
            connect(_signal, callback) {this.toggle = callback;}
        },
    },
    Main: {panel: {addToStatusArea() {}}, notifyError() {throw new Error('Unexpected panel error');}},
};
for (const file of ['client', 'indicator']) {
    const source = fs.readFileSync(new URL(`../packaging/gnome-extension/${file}.js`, import.meta.url), 'utf8')
        .replace(/^import .*;\n/gm, '').replace(/^export /gm, '');
    vm.runInNewContext(source + (file === 'client' ? '\nglobalThis.TestClient = Client;' : '\nglobalThis.TestIndicator = Indicator;'), context);
}
const panel = new context.TestIndicator();
await new Promise(setImmediate);
assert.equal(calls[0].flags, 1, 'panel reads must not override disabled login startup');
assert.equal(panel._status.label.text, 'Receiving from Mac');
await panel._run({command: 'set_sharing', enabled: false});
await new Promise(setImmediate);
assert.equal(panel._status.label.text, 'Sharing paused');
assert.equal(panel._sharing.state, false);
assert.equal(panel._icon.icon_name, 'media-playback-pause-symbolic');
// An agent from another API level gets "Update zflow" instead of a broken menu.
snapshot.api = 2;
await panel._client.refresh();
assert.equal(panel._status.label.text, 'Update zflow');
assert.equal(panel._icon.icon_name, 'dialog-warning-symbolic');
snapshot.api = 1;
await panel._client.refresh();
assert.equal(panel._status.label.text, 'Sharing paused');
panel.destroy();
assert.equal(timers.size, 0, 'disable removes panel polling');
assert.ok(panel._button.destroyed);
// Shell destroys the panel at session end without calling disable().
const ended = new context.TestIndicator();
Object.freeze(ended._status.label);
ended._button.destroy();
await new Promise(setImmediate);
assert.equal(timers.size, 0, 'session teardown stops panel polling before it reaches disposed widgets');
ended.destroy();
assert.equal(ended._button.destroyed, 1, 'a later disable leaves the disposed button alone');
const settings = new context.TestClient(() => {}, true);
await settings.refresh();
assert.equal(calls.at(-1).flags, 0, 'opening preferences explicitly starts the session agent');
await settings.refresh();
assert.equal(calls.at(-1).flags, 1, 'subsequent reads do not keep restarting a stopped agent');
settings.destroy();
console.log('GNOME panel status, pause, API check, activation policy and cleanup checks passed');
