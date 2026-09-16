import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

// Execute the shipped extension with deterministic compositor operations.
const source = fs.readFileSync(new URL('../packaging/gnome-extension/extension.js', import.meta.url), 'utf8')
    .replace(/^import .*;\n/gm, '').replace('export default class ', 'class ') + '\nglobalThis.TestExtension = ZflowExtension;';

function desktop(monitors = [{x: 0, y: 0, width: 1920, height: 1080}]) {
    let pointer = {x: monitors[0].x + 100, y: monitors[0].y + 100};
    let now = 1;
    const barriers = [];
    const timers = new Map();
    const handlers = new Map();
    let next = 1;
    const sessionMode = {isLocked: false, isGreeter: false};
    const context = {
        Extension: class {},
        Indicator: class { destroy() {} },
        global: {backend: {capabilities: 1}, get_pointer: () => [pointer.x, pointer.y]},
        Main: {sessionMode, layoutManager: {monitors, connect(name, fn) {handlers.set(name, fn); return 1;}, disconnect() {}}},
        GLib: {
            PRIORITY_DEFAULT: 0, PRIORITY_DEFAULT_IDLE: 0, SOURCE_CONTINUE: true, SOURCE_REMOVE: false,
            get_monotonic_time: () => now,
            timeout_add(_priority, interval, fn) {const id = next++; timers.set(id, {fn, interval, due: now + interval * 1000}); return id;},
            idle_add(_priority, fn) {queueMicrotask(fn); return next++;},
            Source: {remove(id) {timers.delete(id);}},
        },
        Gio: {DBus: {session: {}}, DBusExportedObject: {wrapJSObject() {return {export() {}, unexport() {}};}},
            BusNameOwnerFlags: {NONE: 0}, bus_own_name_on_connection: () => 1, bus_unown_name() {}},
        Clutter: {get_default_backend: () => ({get_default_seat: () => ({warp_pointer(x, y) {pointer = {x, y};}})})},
        Meta: {
            BackendCapabilities: {BARRIERS: 1},
            BarrierDirection: {POSITIVE_X: 1, NEGATIVE_X: 2, POSITIVE_Y: 4, NEGATIVE_Y: 8},
            Barrier: class {
                constructor(properties) {this.properties = properties; barriers.push(this);}
                connect(_name, fn) {this.hit = fn;}
                destroy() {this.destroyed = true;}
            },
        },
    };
    vm.runInNewContext(source, context);
    const extension = new context.TestExtension();
    extension.enable();
    return {extension, barriers, context, sessionMode, handlers, timers, advance(ms) {
        now += ms * 1000;
        for (const [id, timer] of timers) {
            if (timer.due > now) continue;
            if (timer.fn() === context.GLib.SOURCE_REMOVE) timers.delete(id);
            else timer.due = now + timer.interval * 1000;
        }
    }};
}

const prepare = (edge = 'left', extra = {}) => ({command: 'prepare', token: 7, edge, start: 0, end: 1000000, position: 500000, ...extra});

for (const [edge, expectedY, direction] of [['top', 3, 4], ['bottom', 1076, 8]]) {
    const d = desktop();
    const result = await d.extension._request(prepare(edge));
    assert.equal(result.position.x, 960);
    assert.equal(result.position.y, expectedY);
    assert.equal(d.barriers[0].properties.directions, direction);
    d.barriers[0].hit(d.barriers[0], {x: 480, y: edge === 'top' ? 0 : 1080});
    assert.equal((await d.extension._request({command: 'poll', token: 7})).position, 250000);
    d.extension.disable();
}
{
    const d = desktop();
    await assert.rejects(d.extension._request(prepare('left', {token: Number.MAX_SAFE_INTEGER + 1})), /Invalid handoff token/);
    assert.equal((await d.extension._request(prepare('left', {token: Number.MAX_SAFE_INTEGER}))).status, 'prepared');
    d.extension.disable();
}

{
    const d = desktop();
    const result = await d.extension._request(prepare());
    assert.equal(result.position.x, 3);
    assert.equal(result.position.y, 540);
    assert.equal(d.barriers[0].properties.directions, 1, 'left edge permits inward movement');
    const poll = d.extension._request({command: 'poll', token: 7});
    let settled = false;
    poll.then(() => {settled = true;});
    d.advance(199);
    await new Promise(setImmediate);
    assert.equal(settled, false, 'an active poll waits for its hold duration');
    d.advance(1);
    assert.equal((await poll).status, 'active');
    assert.equal(d.timers.size, 1, 'only the lease timer remains after the hold');
    d.barriers[0].hit(d.barriers[0], {x: 0, y: 270});
    assert.equal((await d.extension._request({command: 'poll', token: 7})).position, 250000);
    await assert.rejects(d.extension._request({command: 'finish', token: 8}), /expired|ended/);
    assert.equal((await d.extension._request({command: 'poll', token: 7})).status, 'returned', 'a stale token cannot clear the active lease');
    d.extension.disable();
}
{
    const d = desktop();
    await d.extension._request(prepare());
    const poll = d.extension._request({command: 'poll', token: 7});
    assert.equal(d.timers.size, 2);
    d.barriers[0].hit(d.barriers[0], {x: 0, y: 270});
    assert.equal((await poll).position, 250000, 'a pending poll returns the barrier position without advancing time');
    assert.equal(d.timers.size, 1, 'the barrier hit removes the hold timer');
    assert.equal(d.extension._lease.polls.size, 0);
    d.extension.disable();
}
{
    const d = desktop();
    await d.extension._request(prepare());
    const poll = d.extension._request({command: 'poll', token: 7});
    const rejected = assert.rejects(poll, /expired|ended/);
    // A delayed main loop can observe lease expiry before the hold callback.
    d.advance(2001);
    await rejected;
    assert.equal(d.timers.size, 1, 'lease expiry removes the pending hold timer');
    assert.ok(d.barriers.every(b => b.destroyed));
    d.extension.disable();
}
{
    const d = desktop();
    await d.extension._request(prepare('right'));
    assert.equal(d.barriers[0].properties.x1, 1920);
    assert.equal(d.barriers[0].properties.directions, 2, 'right edge permits negative X');
    const poll = d.extension._request({command: 'poll', token: 7});
    const rejected = assert.rejects(poll, /expired|ended/);
    const result = await d.extension._request({command: 'finish', token: 7});
    await rejected;
    assert.equal(d.timers.size, 1, 'finish removes the pending hold timer');
    assert.equal(result.status, 'finished');
    assert.ok(d.barriers.every(b => b.destroyed));
    await assert.rejects(d.extension._request({command: 'poll', token: 7}), /expired|ended/);
    d.extension.disable();
}
{
    const d = desktop([{x: -200, y: -100, width: 200, height: 100}, {x: -200, y: 100, width: 200, height: 100}]);
    await assert.rejects(d.extension._request(prepare('left')), /gap/);
    assert.equal(d.barriers.length, 0);
    const result = await d.extension._request(prepare('left', {position: 100000}));
    assert.equal(result.position.x, -197);
    assert.equal(result.position.y, -70);
    assert.equal(d.barriers.length, 2);
    d.extension.disable();
}
{
    const d = desktop();
    await d.extension._request(prepare());
    d.advance(2001);
    assert.ok(d.barriers.every(b => b.destroyed));
    await assert.rejects(d.extension._request({command: 'poll', token: 7}), /expired|ended/);
    await d.extension._request(prepare('top', {token: 8}));
    d.sessionMode.isLocked = true;
    d.advance(250);
    assert.ok(d.barriers.every(b => b.destroyed));
    await assert.rejects(d.extension._request(prepare()), /Unlock/);
    d.extension.disable();
}
{
    const d = desktop();
    d.context.Clutter.get_default_backend = () => ({get_default_seat: () => ({warp_pointer() {}})});
    await assert.rejects(d.extension._request(prepare()), /did not place/);
    assert.ok(d.barriers.every(b => b.destroyed));
    assert.equal(d.extension._lease, null);
    d.extension.disable();
}
{
    const d = desktop();
    await d.extension._request(prepare());
    d.handlers.get('monitors-changed')();
    assert.ok(d.barriers.every(b => b.destroyed));
    await assert.rejects(d.extension._request({command: 'poll', token: 7}), /expired|ended/);
    d.extension.disable();
}
{
    const d = desktop();
    const idle = [];
    d.context.GLib.idle_add = (_priority, fn) => {idle.push(fn); return idle.length;};
    const old = d.extension._request(prepare());
    d.handlers.get('monitors-changed')();
    const replacement = d.extension._request(prepare('right', {token: 8}));
    idle[0]();
    await assert.rejects(old, /changed/);
    assert.equal(d.extension._lease.token, 8, 'cancelled entry cannot clear a newer handoff');
    idle[1]();
    assert.equal((await replacement).status, 'prepared');
    d.extension.disable();
}
console.log('GNOME desktop entry, return, geometry, stale requests, lease, lock and placement checks passed');
