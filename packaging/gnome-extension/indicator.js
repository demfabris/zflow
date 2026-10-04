import St from 'gi://St';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Client, compatible, needsAttention, statusText} from './client.js';

export class Indicator {
    constructor() {
        this._button = new PanelMenu.Button(0.0, 'zflow');
        this._icon = new St.Icon({icon_name: 'io.zflow.zflow-symbolic', style_class: 'system-status-icon'});
        this._button.add_child(this._icon);
        this._status = new PopupMenu.PopupMenuItem('Starting zflow…', {reactive: false});
        this._button.menu.addMenuItem(this._status);
        this._button.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._sharing = new PopupMenu.PopupSwitchMenuItem('Input Sharing', false);
        this._sharing.setSensitive(false);
        this._button.menu.addMenuItem(this._sharing);
        // GNOME 50 emits toggled for setToggleState too, so only a person's
        // flip may change sharing.
        this._updating = false;
        this._sharing.connect('toggled', (_item, enabled) => {
            if (!this._updating) this._run({command: 'set_sharing', enabled});
        });
        this._button.menu.addAction('Settings…', () => this._run({command: 'open_settings'}));
        this._busy = false;
        this._destroyed = false;
        this._client = new Client(snapshot => this._update(snapshot));
        // Shell destroys the panel at session end without calling disable().
        this._button.connect('destroy', () => {
            this._destroyed = true;
            this._client.destroy();
        });
        Main.panel.addToStatusArea('zflow', this._button);
        this._client.start();
    }

    _update(snapshot) {
        const title = statusText(snapshot);
        this._status.label.text = title;
        this._button.accessible_name = `zflow, ${title}`;
        // The service answers sharing as null while it cannot be reached.
        const sharing = compatible(snapshot) ? snapshot.sharing : null;
        this._updating = true;
        this._sharing.setToggleState(sharing ?? false);
        this._updating = false;
        this._sharing.setSensitive(sharing !== null && !this._busy);
        this._icon.icon_name = needsAttention(snapshot) ? 'dialog-warning-symbolic'
            : sharing ? 'io.zflow.zflow-symbolic' : 'media-playback-pause-symbolic';
    }

    async _run(request) {
        if (this._busy) return;
        this._busy = true;
        this._sharing.setSensitive(false);
        try { await this._client.call(request); }
        catch (error) { if (!this._destroyed) Main.notifyError('zflow', error.message); }
        finally {
            this._busy = false;
            if (!this._destroyed) this._client.refresh();
        }
    }

    destroy() {
        if (!this._destroyed) this._button.destroy();
    }
}
