import Adw from 'gi://Adw?version=1';
import Gtk from 'gi://Gtk?version=4.0';
import {Client, statusText} from './client.js';

function button(label, action, css = []) {
    const widget = new Gtk.Button({label, valign: Gtk.Align.CENTER, css_classes: css});
    widget.connect('clicked', action);
    return widget;
}

export class Settings {
    constructor(window) {
        this.window = window;
        this._updating = false;
        this._busy = false;
        this._disposed = false;
        this._peerKey = '';
        this._peerRows = [];
        this.page = new Adw.PreferencesPage({title: 'zflow', icon_name: 'input-mouse-symbolic'});
        const sharing = new Adw.PreferencesGroup();
        this._status = new Adw.ActionRow({title: 'Starting zflow…', subtitle: 'Share your keyboard, pointer, and trackpad.', subtitle_lines: 3, use_markup: false});
        this._statusIcon = new Gtk.Image({icon_name: 'input-mouse-symbolic'});
        this._status.add_prefix(this._statusIcon);
        sharing.add(this._status);
        this._sharing = new Adw.SwitchRow({title: 'Input Sharing', subtitle: 'Allow input to move between paired computers.', sensitive: false});
        this._sharing.connect('notify::active', () => {
            if (!this._updating) this._run({command: 'set_sharing', enabled: this._sharing.active});
        });
        sharing.add(this._sharing);
        this.page.add(sharing);

        this._computers = new Adw.PreferencesGroup({title: 'Computers', description: 'Arrange computers in zflow on the sending Mac.'});
        this._pairButton = button('Pair Computer…', () => this._openPairing(), ['suggested-action']);
        this._computers.header_suffix = this._pairButton;
        this.page.add(this._computers);
        const preferences = new Adw.PreferencesGroup();
        this._login = new Adw.SwitchRow({title: 'Start at Login', subtitle: 'Keep zflow available after you close settings.'});
        this._login.connect('notify::active', () => {
            if (!this._updating) this._run({command: 'set_autostart', enabled: this._login.active});
        });
        preferences.add(this._login);
        this.page.add(preferences);
        this._errorGroup = new Adw.PreferencesGroup({visible: false});
        this._error = new Adw.ActionRow({title: 'Needs attention', subtitle_lines: 0});
        this._error.add_prefix(new Gtk.Image({icon_name: 'dialog-warning-symbolic'}));
        this._errorGroup.add(this._error);
        this.page.add(this._errorGroup);
        const help = new Adw.PreferencesGroup({description: 'Changes save automatically. Advanced input and network settings remain in /etc/zflow/zflow.toml.'});
        this.page.add(help);
        this.client = new Client(snapshot => this._update(snapshot), true);
        this.client.start();
    }

    _showError(message) {
        if (this._disposed) return;
        this._actionError = message;
        this._error.subtitle = message ?? '';
        this._errorGroup.visible = !!message;
    }

    async _run(request) {
        if (this._busy) return false;
        this._busy = true;
        this._sharing.sensitive = this._login.sensitive = this._pairButton.sensitive = false;
        this._showError(null);
        let success = false;
        try { await this.client.call(request); success = true; }
        catch (error) { this._showError(error.message); }
        finally {
            this._busy = false;
            if (!this._disposed) await this.client.refresh();
        }
        return success;
    }

    _update(snapshot) {
        if (this._disposed) return;
        this._updating = true;
        const daemon = snapshot.daemon;
        this._status.title = statusText(snapshot);
        this._status.subtitle = snapshot.error || (!snapshot.desktop_ready && daemon?.sharing ? snapshot.desktop : 'Share your keyboard, pointer, and trackpad.');
        this._statusIcon.icon_name = !daemon || (!snapshot.desktop_ready && daemon.sharing) ? 'dialog-warning-symbolic' : 'input-mouse-symbolic';
        this._sharing.active = daemon?.sharing ?? false;
        this._sharing.sensitive = !!daemon && !this._busy;
        this._login.active = snapshot.autostart ?? false;
        this._login.sensitive = snapshot.autostart !== undefined && !this._busy;
        this._pairButton.sensitive = !!daemon && !this._busy && !this._pairing;
        this._updating = false;
        this._error.subtitle = this._actionError || snapshot.error || '';
        this._errorGroup.visible = !!this._error.subtitle;
        // Keep rows and keyboard focus stable while unchanged snapshots arrive.
        const key = JSON.stringify([daemon?.peers, daemon?.connected, daemon?.receiving_from, daemon?.sending_to]);
        if (key !== this._peerKey) {
            this._peerKey = key;
            for (const row of this._peerRows) this._computers.remove(row);
            this._peerRows = [];
            for (const [name] of Object.entries(daemon?.peers ?? {})) {
                const detail = daemon.receiving_from === name ? 'Receiving input' : daemon.sending_to === name ? 'Controlling this computer'
                    : daemon.connected.includes(name) ? 'Connected' : 'Paired';
                const row = new Adw.ActionRow({title: name, subtitle: detail, use_markup: false});
                row.add_prefix(new Gtk.Image({icon_name: 'computer-symbolic'}));
                const forget = new Gtk.Button({icon_name: 'user-trash-symbolic', tooltip_text: `Forget ${name}`, valign: Gtk.Align.CENTER, css_classes: ['flat']});
                forget.connect('clicked', () => this._forget(name));
                row.add_suffix(forget);
                this._computers.add(row);
                this._peerRows.push(row);
            }
            if (!this._peerRows.length) {
                const row = new Adw.ActionRow({title: 'No paired computers', subtitle: 'Pair your Mac or another Linux computer to get started.'});
                this._computers.add(row);
                this._peerRows.push(row);
            }
        }
        this._updatePairing(snapshot);
    }

    _forget(name) {
        const dialog = new Adw.AlertDialog({heading: `Forget ${name}?`, body: 'This stops input from this computer and removes its trusted identity. Pair it again to reconnect.'});
        dialog.add_response('cancel', 'Cancel');
        dialog.add_response('forget', 'Forget Computer');
        dialog.set_response_appearance('forget', Adw.ResponseAppearance.DESTRUCTIVE);
        dialog.default_response = dialog.close_response = 'cancel';
        dialog.connect('response', (_dialog, response) => {
            if (response === 'forget') this._run({command: 'forget', name});
        });
        dialog.present(this.window);
    }

    _openPairing() {
        if (this._pairing) return;
        const dialog = new Adw.Dialog({title: 'Pair Computer', content_width: 440, content_height: 520});
        const toolbar = new Adw.ToolbarView();
        toolbar.add_top_bar(new Adw.HeaderBar());
        const page = new Adw.PreferencesPage();
        toolbar.content = page;
        dialog.child = toolbar;
        const setup = new Adw.PreferencesGroup({title: 'Connect a computer', description: 'On your Mac, open zflow settings and choose Pair Computer. Then choose this computer.'});
        const listen = button('Wait for Connection', () => this._startPair(null), ['suggested-action']);
        setup.add(listen);
        const remote = new Adw.EntryRow({title: 'IP address and port', text: ''});
        setup.add(remote);
        setup.add(button('Connect to Address', () => this._startPair(remote.text.trim())));
        page.add(setup);
        const nearby = new Adw.PreferencesGroup({title: 'Nearby computers', description: 'Open pairing on the other computer before connecting.'});
        page.add(nearby);
        const progress = new Adw.PreferencesGroup({visible: false});
        const stage = new Adw.ActionRow({title: 'Waiting for another computer…', subtitle_lines: 0});
        progress.add(stage);
        page.add(progress);
        const confirmation = new Adw.PreferencesGroup({title: 'Confirm on both computers', visible: false, description: 'Enter the code shown on the other computer. Enter this computer’s code on the other side.'});
        const code = new Adw.ActionRow({title: '', subtitle: 'This computer’s code'});
        code.add_css_class('numeric');
        confirmation.add(code);
        const name = new Adw.EntryRow({title: 'Computer name'});
        const entered = new Adw.EntryRow({title: 'Other computer’s six-digit code', input_purpose: Gtk.InputPurpose.DIGITS});
        confirmation.add(name);
        confirmation.add(entered);
        const confirm = button('Pair Computer', () => this._run({command: 'pair_confirm', name: name.text.trim(), code: entered.text.trim()}), ['suggested-action']);
        confirmation.add(confirm);
        page.add(confirmation);
        const issueGroup = new Adw.PreferencesGroup();
        const issue = new Adw.ActionRow({title: 'Pairing needs attention', visible: false, subtitle_lines: 0});
        issueGroup.add(issue);
        page.add(issueGroup);
        const cancel = button('Cancel Pairing', async () => {
            if (await this._run({command: 'pair_cancel'})) this._pairOwned = false;
        });
        progress.add(cancel);
        this._pairing = {dialog, setup, progress, stage, confirmation, code, name, entered, confirm, nearby, rows: [], nearbyKey: '', lastState: '', issue, cancel};
        dialog.connect('closed', () => {
            this._pairing = null;
            if (this._pairOwned) {
                this._pairOwned = false;
                this.client.call({command: 'pair_cancel'}).catch(error => this._showError(error.message));
            }
            this.client.refresh();
        });
        dialog.present(this.window);
        this._updatePairing(this.client.snapshot ?? {});
    }

    async _startPair(remote) {
        if (remote !== null && !remote) { this._showError('Enter an IP address and port, for example 192.168.1.20:43120.'); return; }
        this._pendingPair = this._run({command: 'pair', remote});
        const started = await this._pendingPair;
        this._pendingPair = null;
        if (started && (!this._pairing || this._disposed)) {
            await this.client.call({command: 'pair_cancel'}).catch(() => {});
            return;
        }
        this._pairOwned = started;
    }

    _updatePairing(snapshot) {
        const ui = this._pairing;
        if (!ui) return;
        const pairing = snapshot.pairing ?? {state: 'idle'};
        const active = ['waiting', 'confirm', 'saving'].includes(pairing.state);
        ui.setup.visible = !active && pairing.state !== 'paired';
        ui.nearby.visible = ui.setup.visible;
        ui.progress.visible = active || pairing.state === 'paired';
        ui.confirmation.visible = pairing.state === 'confirm' || pairing.state === 'saving';
        ui.confirm.sensitive = pairing.state === 'confirm' && !this._busy;
        ui.cancel.visible = active;
        ui.stage.title = {waiting: 'Waiting for another computer…', confirm: 'Check the codes on both computers', saving: 'Pairing…', paired: 'Computer paired'}[pairing.state] ?? '';
        ui.stage.subtitle = pairing.state === 'paired' ? 'You can close this window and arrange your computers on the sending Mac.' : '';
        if (pairing.state === 'confirm' && ui.lastState !== 'confirm') {
            ui.code.title = pairing.code ?? '';
            ui.name.text = pairing.name ?? '';
            ui.entered.text = '';
        }
        ui.lastState = pairing.state;
        ui.issue.visible = !!(pairing.error || this._actionError);
        ui.issue.subtitle = pairing.error || this._actionError || '';
        const key = JSON.stringify([snapshot.nearby, snapshot.discovery_error]);
        if (key !== ui.nearbyKey) {
            ui.nearbyKey = key;
            for (const row of ui.rows) ui.nearby.remove(row);
            ui.rows = [];
            for (const record of snapshot.nearby ?? []) {
                const address = record.addresses[0];
                const row = new Adw.ActionRow({title: address, subtitle: record.compatible ? 'Available on your network' : 'Update zflow on this computer', use_markup: false});
                const connect = button('Connect', () => {
                    const separator = address.lastIndexOf(':');
                    this._startPair(`${address.slice(0, separator)}:43120`);
                });
                connect.sensitive = record.compatible;
                row.add_suffix(connect);
                ui.nearby.add(row);
                ui.rows.push(row);
            }
            if (!ui.rows.length) {
                const row = new Adw.ActionRow({title: 'No computers found', subtitle: snapshot.discovery_error || 'You can wait for a connection or enter an address.', subtitle_lines: 0});
                ui.nearby.add(row);
                ui.rows.push(row);
            }
        }
    }

    destroy() {
        this._disposed = true;
        // Let the cancellation reach the service before cancelling pending UI reads.
        if (this._pendingPair) this._pendingPair.then(started => started ? this.client.call({command: 'pair_cancel'}) : null).finally(() => this.client.destroy()).catch(() => {});
        else if (this._pairOwned) this.client.call({command: 'pair_cancel'}).finally(() => this.client.destroy()).catch(() => {});
        else this.client.destroy();
    }
}
