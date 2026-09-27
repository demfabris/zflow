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
        // A fresh install opens pairing with its code on screen, so the Mac
        // can pair without anyone clicking through settings here.
        if (daemon && !this._checkedFirstRun) {
            this._checkedFirstRun = true;
            if (!Object.keys(daemon.peers ?? {}).length) this._openPairing();
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
        const dialog = new Adw.Dialog({title: 'Pair Computer', content_width: 440, content_height: 560});
        const toolbar = new Adw.ToolbarView();
        toolbar.add_top_bar(new Adw.HeaderBar());
        const page = new Adw.PreferencesPage();
        toolbar.content = page;
        dialog.child = toolbar;
        // Knowing the code is not enough: the person here allows the computer.
        const ask = new Adw.PreferencesGroup({visible: false});
        const question = new Adw.ActionRow({title: '', subtitle_lines: 0, use_markup: false});
        question.add_prefix(new Gtk.Image({icon_name: 'computer-symbolic'}));
        ask.add(question);
        const allow = button('Allow', () => this._respond(true), ['suggested-action']);
        const answers = new Gtk.Box({spacing: 12, halign: Gtk.Align.END, margin_top: 12});
        answers.append(button('Decline', () => this._respond(false)));
        answers.append(allow);
        ask.add(answers);
        page.add(ask);
        const shown = new Adw.PreferencesGroup({title: 'Setup code', description: 'On your Mac, open zflow, choose this computer, and type this code.'});
        const code = new Gtk.Label({label: '', selectable: true, css_classes: ['title-1', 'numeric'], margin_top: 18, margin_bottom: 18});
        shown.add(code);
        const renew = button('Show a New Code', () => this._listen());
        shown.add(renew);
        page.add(shown);
        const other = new Adw.PreferencesGroup({title: 'Or type another computer’s code', description: 'Open Pair Computer on the other computer, then enter its address and the code it shows.'});
        const remote = new Adw.EntryRow({title: 'IP address'});
        const entered = new Adw.EntryRow({title: 'Its setup code', input_purpose: Gtk.InputPurpose.DIGITS});
        const connect = button('Pair', () => this._connect(remote.text, entered.text), ['suggested-action']);
        other.add(remote);
        other.add(entered);
        other.add(connect);
        page.add(other);
        const nearby = new Adw.PreferencesGroup({title: 'Nearby computers'});
        page.add(nearby);
        const result = new Adw.PreferencesGroup();
        // Peer names and remote error text end up here, so never parse them as markup.
        const stage = new Adw.ActionRow({title: '', visible: false, subtitle_lines: 0, use_markup: false});
        result.add(stage);
        page.add(result);
        this._pairing = {dialog, ask, question, allow, shown, other, code, renew, remote, entered, connect, stage, nearby, rows: [], nearbyKey: '', asked: false};
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
        // Showing a code is the usual case: the Mac types it.
        this._listen();
    }

    _listen() {
        return this._startPair({command: 'pair', remote: null});
    }

    _respond(allow) {
        return this._run({command: 'pair_respond', allow});
    }

    _connect(remote, code) {
        remote = remote.trim();
        if (!remote) { this._showError('Enter the other computer’s IP address.'); return Promise.resolve(false); }
        return this._startPair({command: 'pair', remote, code: code.replace(/[\s-]/g, '')});
    }

    async _startPair(request) {
        // One pairing runs at a time, so a new request replaces the code on screen.
        if (this._pairOwned) {
            this._pairOwned = false;
            await this.client.call({command: 'pair_cancel'}).catch(() => {});
        }
        this._pendingPair = this._run(request);
        const started = await this._pendingPair;
        this._pendingPair = null;
        if (started && (!this._pairing || this._disposed)) {
            await this.client.call({command: 'pair_cancel'}).catch(() => {});
            return false;
        }
        this._pairOwned = started;
        return started;
    }

    _updatePairing(snapshot) {
        const ui = this._pairing;
        if (!ui) return;
        const pairing = snapshot.pairing ?? {state: 'idle'};
        const listening = pairing.state === 'listening';
        const confirming = pairing.state === 'confirm';
        const connecting = pairing.state === 'connecting' || pairing.state === 'approving';
        ui.ask.visible = confirming;
        ui.shown.visible = ui.other.visible = !confirming;
        if (confirming) {
            ui.question.title = `Allow ${pairing.name ?? 'this computer'} to control this computer?`;
            ui.question.subtitle = `It entered this computer’s code from ${pairing.address ?? 'your network'}. Allow it only if it is the computer you are setting up.`;
            if (!ui.asked) ui.allow.grab_focus();
        }
        ui.asked = confirming;
        ui.code.label = listening ? (pairing.code ?? '…') : 'No code shown';
        ui.code.sensitive = listening;
        ui.renew.visible = !listening && !connecting && !confirming;
        ui.connect.sensitive = !connecting && !this._busy;
        const issue = pairing.error || this._actionError;
        ui.stage.visible = connecting || pairing.state === 'paired' || !!issue;
        ui.stage.title = pairing.state === 'approving' ? 'Waiting for the other computer…' : connecting ? 'Pairing…' : pairing.state === 'paired' ? `Paired with ${pairing.name ?? 'the other computer'}` : 'Pairing needs attention';
        ui.stage.subtitle = pairing.state === 'paired' ? 'You can close this window. Arrange your computers in zflow on the Mac.'
            : pairing.state === 'approving' ? 'Choose Allow on the other computer.' : connecting ? '' : issue || '';
        const key = JSON.stringify([snapshot.nearby, snapshot.discovery_error]);
        if (key !== ui.nearbyKey) {
            ui.nearbyKey = key;
            for (const row of ui.rows) ui.nearby.remove(row);
            ui.rows = [];
            for (const record of snapshot.nearby ?? []) {
                const address = record.addresses[0];
                const separator = address.lastIndexOf(':');
                const row = new Adw.ActionRow({title: address.slice(0, separator), subtitle: record.compatible ? 'Available on your network' : 'Update zflow on this computer', use_markup: false});
                const use = button('Use', () => {
                    // Receivers advertise their input port; pairing listens on 43120.
                    ui.remote.text = `${address.slice(0, separator)}:43120`;
                    ui.entered.grab_focus();
                });
                use.sensitive = record.compatible;
                row.add_suffix(use);
                ui.nearby.add(row);
                ui.rows.push(row);
            }
            if (!ui.rows.length) {
                const row = new Adw.ActionRow({title: 'No other computers found', subtitle: snapshot.discovery_error || 'You can enter an address instead.', subtitle_lines: 0});
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
