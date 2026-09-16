import St from 'gi://St';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Client, statusText} from './client.js';

export class Indicator {
    constructor() {
        this._button = new PanelMenu.Button(0.0, 'zflow');
        this._icon = new St.Icon({icon_name: 'input-mouse-symbolic', style_class: 'system-status-icon'});
        this._button.add_child(this._icon);
        this._status = new PopupMenu.PopupMenuItem('Starting zflow…', {reactive: false});
        this._button.menu.addMenuItem(this._status);
        this._button.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._sharing = new PopupMenu.PopupSwitchMenuItem('Input Sharing', false);
        this._sharing.setSensitive(false);
        this._button.menu.addMenuItem(this._sharing);
        this._sharing.connect('toggled', (_item, enabled) => this._run({command: 'set_sharing', enabled}));
        this._button.menu.addAction('Settings…', () => this._run({command: 'open_settings'}));
        this._busy = false;
        this._destroyed = false;
        this._client = new Client(snapshot => this._update(snapshot));
        Main.panel.addToStatusArea('zflow', this._button);
        this._client.start();
    }

    _update(snapshot) {
        const title = statusText(snapshot);
        this._status.label.text = title;
        this._button.accessible_name = `zflow, ${title}`;
        this._sharing.setToggleState(snapshot.daemon?.sharing ?? false);
        this._sharing.setSensitive(!!snapshot.daemon && !this._busy);
        this._icon.icon_name = !snapshot.daemon || (snapshot.daemon.sharing && !snapshot.desktop_ready)
            ? 'dialog-warning-symbolic' : snapshot.daemon.sharing ? 'input-mouse-symbolic' : 'media-playback-pause-symbolic';
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
        this._destroyed = true;
        this._client.destroy();
        this._button.destroy();
    }
}
