"""OpenSilexLink GUI (PySide6) - main window in the style of SX Virtual Link.

Shows discovered Silex device servers as a tree with their USB devices and status
(Available / Connected by me / In use by <host>), and offers Connect/Disconnect.
The daemon does the real work; the GUI only speaks IPC and starts the daemon on
demand if needed.
"""

from __future__ import annotations

import os
import shlex
import shutil
import subprocess
import sys
import threading
import time

from PySide6.QtCore import Qt, QObject, Signal
from PySide6.QtWidgets import (
    QApplication, QMainWindow, QTreeWidget, QTreeWidgetItem, QToolBar,
    QPushButton, QWidget, QVBoxLayout, QHBoxLayout, QMessageBox, QStyle, QMenu,
    QSystemTrayIcon, QCheckBox,
)
from PySide6.QtGui import QAction, QIcon

from .. import __version__, ipc
from .. import i18n
from ..i18n import _
from ..settings import SettingsStore
from . import autostart
from .client import (
    DaemonClient, status_text, server_label, can_connect, can_disconnect,
    can_request, request_status_text, tray_summary, tray_device_status,
    notice_text, notice_is_error, visible_servers,
    resolve_columns, column_labels, device_cell, device_icon_candidates, SERVER_ICON,
    display_name, connected_keys, apps_to_launch,
)
from .options import OptionsDialog, DevicePropertiesDialog

_ROLE = Qt.UserRole

# Brand name (kept untranslated).
APP_TITLE = "Silex USB Device Server"
PROJECT_URL = "https://github.com/bytewarrior/OpenSilexLink"

# The daemon executable (the Rust silex-daemon binary).
DAEMON_BIN = "silex-daemon"


class _Bridge(QObject):
    """Marshals messages from IPC/worker threads into the Qt thread."""
    servers = Signal(list)
    op_done = Signal(str, bool, str)      # (action, ok, message)
    daemon_lost = Signal()
    access_request = Signal(dict)         # incoming request from another host (prompt)
    access_status = Signal(dict)          # status of our own request
    notice = Signal(dict)                 # connection notice (auto-reconnect etc.)
    app_closed = Signal(str, str)         # launched app has exited -> disconnect (mac, dev_id)


class MainWindow(QMainWindow):
    def __init__(self, address=None):
        super().__init__()
        self.setWindowTitle(APP_TITLE)
        self.resize(560, 480)
        self._address = address or ipc.default_socket_path()
        self._bridge = _Bridge()
        self._bridge.servers.connect(self._render)
        self._bridge.op_done.connect(self._on_op_done)
        self._bridge.daemon_lost.connect(self._on_daemon_lost)
        self._bridge.access_request.connect(self._on_access_request)
        self._bridge.access_status.connect(self._on_access_status)
        self._bridge.notice.connect(self._on_notice)
        self._bridge.app_closed.connect(self._on_app_closed)
        self._servers: list[dict] = []
        self._notify_errors = True        # setting "notify on error"
        self._confirm_disconnect = True   # confirm before disconnecting
        self._hide_on_close = True        # close -> tray instead of quitting
        self._filter_enabled = False      # display filter (only checked servers)
        self._filter_servers: list[str] = []
        self._columns = list(resolve_columns(None))   # visible columns (options tab 3)
        self._column_widths: dict = {}
        self._flat_view = False           # False = device view (tree), True = server view (table)
        self.client: DaemonClient | None = None
        self._tray: QSystemTrayIcon | None = None
        self._tray_hint_shown = False    # show the "keeps running" balloon only once
        self._quitting = False           # real quit vs. minimize to tray
        self._act_restore = None         # tray menu entries Restore/Minimize
        self._act_minimize = None        # (enable state synced via aboutToShow)
        self._connected_prev: set = set()   # previously connected devices (for app launch)
        self._launch_baseline = False    # first render = baseline, no auto-launch
        self._launched: dict = {}        # (mac,dev_id) -> launched process

        self._build_ui()
        self._build_tray()
        self._start()

    # -- UI construction --------------------------------------------------- #
    def _build_ui(self) -> None:
        style = self.style()
        tb = QToolBar()
        tb.setMovable(False)
        self.addToolBar(tb)
        act_refresh = QAction(style.standardIcon(QStyle.SP_BrowserReload), _("Refresh"), self)
        act_refresh.triggered.connect(self._refresh)
        self.act_toggle = QAction(style.standardIcon(QStyle.SP_FileDialogListView),
                                  _("Toggle views"), self)
        self.act_toggle.setToolTip(_("Toggle views (Alt+S)\nSwitches between the "
                                     "USB device view and the device server view."))
        self.act_toggle.setShortcut("Alt+S")
        self.act_toggle.setCheckable(True)
        self.act_toggle.toggled.connect(self._toggle_view)
        act_settings = QAction(style.standardIcon(QStyle.SP_FileDialogDetailedView), _("Settings"), self)
        act_settings.triggered.connect(self._settings)
        act_help = QAction(style.standardIcon(QStyle.SP_MessageBoxQuestion), _("Help"), self)
        act_help.triggered.connect(self._help)
        tb.addAction(act_refresh)
        tb.addAction(self.act_toggle)
        tb.addAction(act_settings)
        tb.addAction(act_help)

        self.tree = QTreeWidget()
        self.tree.itemSelectionChanged.connect(self._update_buttons)
        self.tree.itemDoubleClicked.connect(lambda *_a: self._toggle_selected())
        self.tree.setContextMenuPolicy(Qt.CustomContextMenu)
        self.tree.customContextMenuRequested.connect(self._context_menu)

        self.btn_connect = QPushButton(_("Connect"))
        self.btn_connect.clicked.connect(self._do_connect)
        self.btn_disconnect = QPushButton(_("Disconnect"))
        self.btn_disconnect.clicked.connect(self._do_disconnect)
        self.btn_connect.setEnabled(False)
        self.btn_disconnect.setEnabled(False)

        buttons = QHBoxLayout()
        buttons.addStretch(1)
        buttons.addWidget(self.btn_connect)
        buttons.addWidget(self.btn_disconnect)
        buttons.addStretch(1)

        central = QWidget()
        lay = QVBoxLayout(central)
        lay.addWidget(self.tree, 1)
        lay.addLayout(buttons)
        self.setCentralWidget(central)
        self.statusBar().showMessage(_("Connecting to daemon ..."))

    # -- Tray icon --------------------------------------------------------- #
    def _build_tray(self) -> None:
        """System tray icon; the GUI keeps running in the notification area when the
        window is closed (the daemon holds the connections anyway)."""
        if not QSystemTrayIcon.isSystemTrayAvailable():
            return
        icon = self._server_icon()
        self.setWindowIcon(icon)
        self._tray = QSystemTrayIcon(icon, self)
        self._tray.setToolTip(APP_TITLE)
        self._tray.activated.connect(self._on_tray_activated)
        self._rebuild_tray_menu()
        self._tray.show()

    def _rebuild_tray_menu(self) -> None:
        """Tray menu in SX Virtual Link style: devices as submenus, then window/
        program actions (Restore/Minimize/Options/Help/About/Quit)."""
        if self._tray is None:
            return
        menu = QMenu()
        any_dev = False
        for srv in self._visible_servers():
            for dev in srv.get("devices", []):
                any_dev = True
                mac, dev_id = srv.get("mac", ""), dev.get("dev_id", "")
                title = f"{display_name(dev) or '?'} ({tray_device_status(dev)})"
                sub = menu.addMenu(title)
                a_c = sub.addAction(_("Connect"))
                a_c.setEnabled(can_connect(dev))
                a_c.triggered.connect(
                    lambda _c=False, m=mac, d=dev_id: self._tray_device_action("connect", m, d))
                a_d = sub.addAction(_("Disconnect"))
                a_d.setEnabled(can_disconnect(dev))
                a_d.triggered.connect(
                    lambda _c=False, m=mac, d=dev_id: self._tray_device_action("disconnect", m, d))
                a_r = sub.addAction(_("Request use"))
                a_r.setEnabled(can_request(dev))
                a_r.triggered.connect(
                    lambda _c=False, m=mac, d=dev_id: self._tray_device_action("request", m, d))
                sub.addSeparator()
                a_p = sub.addAction(_("Properties..."))
                a_p.triggered.connect(
                    lambda _c=False, m=mac, d=dev_id: self._tray_device_action("properties", m, d))
        if not any_dev:
            menu.addAction(_("No devices found")).setEnabled(False)
        menu.addSeparator()
        self._act_restore = menu.addAction(_("Restore"))
        self._act_restore.triggered.connect(self._show_window)
        self._act_minimize = menu.addAction(_("Minimize"))
        self._act_minimize.triggered.connect(self.hide)
        # Set the enable state whenever the menu opens -> correct regardless of how the
        # window was hidden/shown (tray click, menu, X, ...).
        menu.aboutToShow.connect(self._sync_view_actions)
        self._sync_view_actions()
        menu.addSeparator()
        menu.addAction(_("Options...")).triggered.connect(self._settings)
        menu.addSeparator()
        menu.addAction(_("Help")).triggered.connect(self._help)
        menu.addAction(_("About Silex USB Device Server")).triggered.connect(self._about)
        menu.addSeparator()
        menu.addAction(_("Quit")).triggered.connect(self._quit)
        # Keep a reference, otherwise Qt garbage-collects the menu (and its actions).
        self._tray_menu = menu
        self._tray.setContextMenu(menu)

    def _on_tray_activated(self, reason) -> None:
        if reason in (QSystemTrayIcon.Trigger, QSystemTrayIcon.DoubleClick):
            self._show_window() if not self.isVisible() else self.hide()

    def _show_window(self) -> None:
        self.showNormal()
        self.raise_()
        self.activateWindow()
        self._sync_view_actions()        # refresh Restore/Minimize state

    def _sync_view_actions(self) -> None:
        """Bind the enable state of Restore/Minimize to window visibility."""
        visible = self.isVisible()
        if self._act_restore is not None:
            self._act_restore.setEnabled(not visible)
        if self._act_minimize is not None:
            self._act_minimize.setEnabled(visible)

    def _select_device_in_tree(self, mac: str, dev_id: str) -> bool:
        """Select the tree row (mac, dev_id) so the standard handlers hit it."""
        for i in range(self.tree.topLevelItemCount()):
            node = self.tree.topLevelItem(i)
            for j in range(node.childCount()):
                child = node.child(j)
                if child.data(0, _ROLE) == (mac, dev_id):
                    self.tree.setCurrentItem(child)
                    return True
        return False

    def _tray_device_action(self, kind: str, mac: str, dev_id: str) -> None:
        """Device action from the tray submenu - selects the row and calls the same
        handler as the context menu (one code path)."""
        if not self._select_device_in_tree(mac, dev_id):
            return
        if kind == "connect":
            self._do_connect()
        elif kind == "disconnect":
            self._do_disconnect()
        elif kind == "request":
            self._do_request()
        elif kind == "properties":
            self._show_window()
            self._open_properties()

    def _notify(self, title: str, message: str) -> None:
        """Tray balloon, so events are visible even when the window is hidden."""
        if self._tray is not None and QSystemTrayIcon.supportsMessages():
            self._tray.showMessage(title, message, QSystemTrayIcon.Information, 4000)

    def closeEvent(self, event) -> None:
        # With a tray + the "hide instead of quit" setting, minimize to the tray
        # (daemon + connections remain). Otherwise really quit the app.
        if not self._quitting and self._tray is not None and self._hide_on_close:
            event.ignore()
            self.hide()
            self._sync_view_actions()    # refresh Restore/Minimize state
            if not self._tray_hint_shown:
                self._tray_hint_shown = True
                self._notify(APP_TITLE,
                             _("Still running in the notification area. Use the tray menu to quit."))
            return
        # Quitting: with an active tray, setQuitOnLastWindowClosed(False) would otherwise
        # prevent quitting -> quit explicitly.
        self._quitting = True
        if self._tray is not None:
            self._tray.hide()
        event.accept()
        QApplication.instance().quit()

    def _quit(self) -> None:
        self._quitting = True
        if self._tray is not None:
            self._tray.hide()
        QApplication.instance().quit()

    # -- Daemon connection ------------------------------------------------- #
    def _start(self) -> None:
        threading.Thread(target=self._connect_daemon, daemon=True).start()

    def _connect_daemon(self) -> None:
        if not self._ensure_daemon():
            self._bridge.op_done.emit("daemon", False,
                                      _("Daemon not reachable and failed to start."))
            return
        try:
            self.client.subscribe()          # initial state arrives as a servers event
            try:
                self._apply_settings_cache(self.client.get_settings())
            except (ipc.IpcError, OSError):
                pass
            self._bridge.op_done.emit("daemon", True, _("Connected to daemon."))
        except (ipc.IpcError, OSError) as exc:
            self._bridge.op_done.emit("daemon", False, str(exc))

    def _apply_settings_cache(self, settings: dict) -> None:
        """Cache the GUI-relevant switches (for prompt/close behaviour)."""
        self._notify_errors = bool(settings.get("notify_errors", self._notify_errors))
        self._confirm_disconnect = bool(settings.get("confirm_disconnect", self._confirm_disconnect))
        self._hide_on_close = bool(settings.get("hide_on_close", self._hide_on_close))
        self._filter_enabled = bool(settings.get("filter_enabled", self._filter_enabled))
        self._filter_servers = list(settings.get("filter_servers", self._filter_servers))
        self._columns = resolve_columns(settings.get("columns", self._columns))
        self._column_widths = dict(settings.get("column_widths", self._column_widths))

    def _make_client(self) -> DaemonClient:
        """Client with all event callbacks (marshalled into the Qt thread)."""
        return DaemonClient(
            self._address,
            on_servers=self._bridge.servers.emit,
            on_access_request=self._bridge.access_request.emit,
            on_access_status=self._bridge.access_status.emit,
            on_notice=self._bridge.notice.emit)

    def _daemon_command(self) -> list[str]:
        """Command line to start the daemon binary for our IPC address."""
        exe = shutil.which(DAEMON_BIN) or DAEMON_BIN
        cmd = [exe]
        if isinstance(self._address, tuple):
            cmd += ["--tcp", f"{self._address[0]}:{self._address[1]}"]
        else:
            cmd += ["--socket", self._address]
        return cmd

    def _ensure_daemon(self) -> bool:
        """Connect to the daemon; start it if needed and wait for the socket."""
        self.client = self._make_client()
        try:
            self.client.connect()
            return True
        except OSError:
            pass
        # Start the daemon itself (desktop: attach/detach via pkexec).
        try:
            subprocess.Popen(self._daemon_command(),
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                             start_new_session=True)
        except OSError:
            return False
        for _try in range(30):
            time.sleep(0.3)
            self.client = self._make_client()
            try:
                self.client.connect()
                return True
            except OSError:
                continue
        return False

    # -- Rendering --------------------------------------------------------- #
    def _visible_servers(self) -> list[dict]:
        """Servers after the display filter (options tab 4); the full list stays in _servers."""
        return visible_servers(self._servers, self._filter_enabled, self._filter_servers)

    def _dev_icon(self, dev: dict) -> QIcon:
        """Device-class icon from the desktop theme: the first candidate the theme
        knows; only if none matches, the Qt standard icon."""
        for name in device_icon_candidates(dev):
            if QIcon.hasThemeIcon(name):
                return QIcon.fromTheme(name)
        return self.style().standardIcon(QStyle.SP_DriveHDIcon)

    def _server_icon(self) -> QIcon:
        return QIcon.fromTheme(SERVER_ICON, self.style().standardIcon(QStyle.SP_DriveNetIcon))

    def _toggle_view(self, flat: bool) -> None:
        """Switch between device view (tree) and device server view (table)."""
        self._flat_view = flat
        self._render(self._servers)

    def _render(self, servers: list) -> None:
        self._servers = servers
        sel = self._selected_ref()
        self.tree.clear()
        if self._flat_view:
            self._render_flat(self._visible_servers(), sel)
        else:
            self._render_tree(self._visible_servers(), sel)
        self._update_buttons()
        if self._tray is not None:
            self._tray.setToolTip(f"{APP_TITLE} - {tray_summary(servers)}")
            self._rebuild_tray_menu()
        self._handle_app_launch(servers)

    # -- App launch on connect / disconnect on app close (client only) ----- #
    def _handle_app_launch(self, servers: list) -> None:
        current = connected_keys(servers)
        if not self._launch_baseline:
            # First state = baseline: do NOT launch apps for already-connected devices.
            self._launch_baseline = True
            self._connected_prev = current
            return
        for spec in apps_to_launch(self._connected_prev, servers):
            key = (spec["mac"], spec["dev_id"])
            if key not in self._launched:
                self._launch_app(spec)
        # Stop tracking disconnected devices as "launched" (a later reconnect is fine).
        for key in list(self._launched):
            if key not in current:
                self._launched.pop(key, None)
        self._connected_prev = current

    def _launch_app(self, spec: dict) -> None:
        key = (spec["mac"], spec["dev_id"])
        try:
            proc = subprocess.Popen(shlex.split(spec["app"]))
        except (OSError, ValueError) as exc:
            self.statusBar().showMessage(_("Could not start the application: {}").format(exc))
            if not self.isVisible():
                self._notify(APP_TITLE, _("Could not start the application."))
            return
        self._launched[key] = proc
        if spec["disconnect_on_app_close"]:
            threading.Thread(target=self._watch_app,
                             args=(spec["mac"], spec["dev_id"], proc), daemon=True).start()

    def _watch_app(self, mac: str, dev_id: str, proc: subprocess.Popen) -> None:
        proc.wait()                                   # blocks until the app exits
        self._bridge.app_closed.emit(mac, dev_id)     # marshal into the Qt thread

    def _on_app_closed(self, mac: str, dev_id: str) -> None:
        """The app launched for this device has exited -> disconnect the device."""
        self._launched.pop((mac, dev_id), None)
        if self.client is None:
            return
        if (mac, dev_id) not in connected_keys(self._servers):
            return                                    # already disconnected elsewhere
        self.statusBar().showMessage(_("Application closed - disconnecting device ..."))
        threading.Thread(
            target=lambda: self._safe(lambda: self.client.disconnect_device(mac, dev_id)),
            daemon=True).start()

    def _render_tree(self, servers: list, sel) -> None:
        """USB device view: a tree grouped by device server (name + status)."""
        self.tree.setColumnCount(2)
        self.tree.setHeaderHidden(True)              # like SX VL (device view without header)
        self.tree.setColumnWidth(0, 320)
        for srv in servers:
            node = QTreeWidgetItem([server_label(srv), ""])
            node.setIcon(0, self._server_icon())
            node.setFirstColumnSpanned(True)
            self.tree.addTopLevelItem(node)
            node.setExpanded(True)
            for dev in srv.get("devices", []):
                item = QTreeWidgetItem([display_name(dev) or "?", status_text(dev)])
                item.setIcon(0, self._dev_icon(dev))
                item.setData(0, _ROLE, (srv.get("mac", ""), dev.get("dev_id", "")))
                node.addChild(item)
                if sel and sel == (srv.get("mac", ""), dev.get("dev_id", "")):
                    item.setSelected(True)
                    self.tree.setCurrentItem(item)

    def _render_flat(self, servers: list, sel) -> None:
        """Device server view: a flat table with the configured columns."""
        cols = resolve_columns(self._columns)
        self.tree.setColumnCount(len(cols))
        self.tree.setHeaderHidden(False)
        self.tree.setHeaderLabels(column_labels(cols))
        for i, c in enumerate(cols):
            w = self._column_widths.get(c)
            self.tree.setColumnWidth(i, int(w) if w else (240 if c == "name" else 120))
        for srv in servers:
            for dev in srv.get("devices", []):
                item = QTreeWidgetItem([device_cell(dev, srv, c) for c in cols])
                item.setIcon(0, self._dev_icon(dev))
                item.setData(0, _ROLE, (srv.get("mac", ""), dev.get("dev_id", "")))
                self.tree.addTopLevelItem(item)
                if sel and sel == (srv.get("mac", ""), dev.get("dev_id", "")):
                    item.setSelected(True)
                    self.tree.setCurrentItem(item)

    def _selected_ref(self):
        item = self.tree.currentItem()
        return item.data(0, _ROLE) if item is not None else None

    def _selected_device(self) -> dict | None:
        return self._lookup(self._selected_ref())[1]

    def _selected_server(self) -> dict | None:
        return self._lookup(self._selected_ref())[0]

    def _lookup(self, ref):
        if not ref:
            return None, None
        mac, dev_id = ref
        for srv in self._servers:
            if srv.get("mac", "") == mac:
                for dev in srv.get("devices", []):
                    if dev.get("dev_id") == dev_id:
                        return srv, dev
        return None, None

    def _update_buttons(self) -> None:
        dev = self._selected_device()
        self.btn_connect.setEnabled(bool(dev) and can_connect(dev))
        self.btn_disconnect.setEnabled(bool(dev) and can_disconnect(dev))

    # -- Actions ----------------------------------------------------------- #
    def _toggle_selected(self) -> None:
        dev = self._selected_device()
        if not dev:
            return
        if can_connect(dev):
            self._do_connect()
        elif can_disconnect(dev):
            self._do_disconnect()

    def _do_connect(self) -> None:
        self._run_op("connect")

    def _do_disconnect(self) -> None:
        if self._confirm_disconnect and not self._confirm_disconnect_dialog():
            return
        self._run_op("disconnect")

    def _confirm_disconnect_dialog(self) -> bool:
        """Confirmation before disconnecting (like SX Virtual Link). True = disconnect."""
        dev = self._selected_device()
        name = (dev or {}).get("description") or _("the device")
        box = QMessageBox(self)
        box.setIcon(QMessageBox.Warning)
        box.setWindowTitle(_("Disconnect device"))
        box.setText(_("Disconnect from {}? Make sure the device is not in use before "
                      "disconnecting.").format(name))
        cb = QCheckBox(_("Do not show a confirmation message before disconnecting"))
        box.setCheckBox(cb)
        box.setStandardButtons(QMessageBox.Yes | QMessageBox.No)
        box.setDefaultButton(QMessageBox.No)
        box.button(QMessageBox.Yes).setText(_("Yes"))
        box.button(QMessageBox.No).setText(_("No"))
        confirmed = box.exec() == QMessageBox.Yes
        if confirmed and cb.isChecked():
            self._set_setting("confirm_disconnect", False)
        return confirmed

    def _set_setting(self, key: str, value) -> None:
        """Persist a single setting immediately (and update the GUI cache)."""
        self._apply_settings_cache({key: value})
        if self.client is None:
            return
        threading.Thread(
            target=lambda: self._safe(lambda: self.client.set_settings({key: value})),
            daemon=True).start()

    def _run_op(self, kind: str) -> None:
        ref = self._selected_ref()
        if not ref or self.client is None:
            return
        mac, dev_id = ref
        self.btn_connect.setEnabled(False)
        self.btn_disconnect.setEnabled(False)
        self.statusBar().showMessage(
            (_("Connecting {} ...") if kind == "connect" else _("Disconnecting {} ...")).format(dev_id))

        def work():
            try:
                if kind == "connect":
                    self.client.connect_device(mac, dev_id)
                else:
                    self.client.disconnect_device(mac, dev_id)
                self._bridge.op_done.emit(kind, True, "")
            except (ipc.IpcError, OSError) as exc:
                self._bridge.op_done.emit(kind, False, str(exc))

        threading.Thread(target=work, daemon=True).start()

    def _on_op_done(self, kind: str, ok: bool, message: str) -> None:
        if kind == "daemon":
            self.statusBar().showMessage(message)
            if not ok:
                QMessageBox.warning(self, "Daemon", message)
            return
        action = _("Connect") if kind == "connect" else _("Disconnect")
        if ok:
            self.statusBar().showMessage(_("Connected.") if kind == "connect" else _("Disconnected."))
            if not self.isVisible():          # window in tray -> show balloon
                self._notify(APP_TITLE,
                             _("Device connected.") if kind == "connect" else _("Device disconnected."))
        else:
            self.statusBar().showMessage(_("Error."))
            if self.isVisible():
                QMessageBox.warning(self, _("Error"),
                                    _("{} failed:\n{}").format(action, message))
            else:
                self._notify(APP_TITLE, _("{} failed.").format(action))
        self._update_buttons()

    def _on_daemon_lost(self) -> None:
        self.statusBar().showMessage(_("Lost connection to the daemon."))

    # -- Request use (send/receive side) ----------------------------------- #
    def _do_request(self) -> None:
        ref = self._selected_ref()
        if not ref or self.client is None:
            return
        mac, dev_id = ref
        self.statusBar().showMessage(_("Requesting {} ...").format(dev_id))

        def work():
            try:
                self.client.request_access(mac, dev_id)
            except (ipc.IpcError, OSError) as exc:
                self._bridge.op_done.emit("request", False, str(exc))

        threading.Thread(target=work, daemon=True).start()

    def _on_access_request(self, data: dict) -> None:
        """Incoming request from another host for a device we hold."""
        requester = data.get("requester") or data.get("requester_ip") or "?"
        desc = data.get("description") or data.get("dev_id") or _("the device")
        if not self.isVisible():          # bring to front from the tray + notify
            self._notify(_("Request use"),
                         _('{} wants to use "{}".').format(requester, desc))
            self._show_window()
        box = QMessageBox(self)
        box.setIcon(QMessageBox.Question)
        box.setWindowTitle(_("Request use"))
        box.setText(_('{} wants to use "{}".\n\n'
                      "Disconnect and release the device?").format(requester, desc))
        box.setStandardButtons(QMessageBox.Yes | QMessageBox.No)
        box.setDefaultButton(QMessageBox.No)
        box.button(QMessageBox.Yes).setText(_("Yes"))
        box.button(QMessageBox.No).setText(_("No"))
        accept = box.exec() == QMessageBox.Yes
        mac, dev_id = data.get("mac", ""), data.get("dev_id", "")
        if self.client is None:
            return
        threading.Thread(
            target=lambda: self._safe(
                lambda: self.client.respond_access_request(mac, dev_id, accept)),
            daemon=True).start()

    def _on_access_status(self, data: dict) -> None:
        """Progress of our own request (access_request_status event)."""
        text = request_status_text(data)
        if text:
            self.statusBar().showMessage(text)
        state = data.get("state")
        if state == "denied":
            QMessageBox.information(self, _("Request use"),
                                    _("The other side denied the request."))
        elif state == "no_response":
            QMessageBox.warning(self, _("Request use"),
                                _("No response from the other side."))
        elif state == "error":
            QMessageBox.warning(self, _("Request use"), text)

    def _on_notice(self, data: dict) -> None:
        """Connection notice (auto-reconnect): always the status bar, a tray balloon
        when the window is hidden - error balloons only if "notify on error"."""
        text = notice_text(data)
        if not text:
            return
        self.statusBar().showMessage(text)
        if not self.isVisible() and (not notice_is_error(data) or self._notify_errors):
            self._notify(APP_TITLE, text)

    def _refresh(self) -> None:
        if self.client is None:
            return
        threading.Thread(target=lambda: self._safe(self.client.refresh), daemon=True).start()

    def _safe(self, fn) -> None:
        try:
            fn()
        except (ipc.IpcError, OSError):
            self._bridge.daemon_lost.emit()

    # -- Context menu ------------------------------------------------------ #
    def _context_menu(self, pos) -> None:
        item = self.tree.itemAt(pos)
        if item is None or item.data(0, _ROLE) is None:
            return
        self.tree.setCurrentItem(item)
        dev = self._selected_device()
        if dev is None:
            return
        ref = self._selected_ref()
        rule = self._rule_for(ref)
        menu = QMenu(self)
        a_c = menu.addAction(_("Connect"))
        a_c.setEnabled(can_connect(dev))
        a_c.triggered.connect(self._do_connect)
        a_d = menu.addAction(_("Disconnect"))
        a_d.setEnabled(can_disconnect(dev))
        a_d.triggered.connect(self._do_disconnect)
        a_req = menu.addAction(_("Request use"))
        a_req.setEnabled(can_request(dev))
        a_req.triggered.connect(self._do_request)
        menu.addSeparator()
        a_auto = menu.addAction(_("Auto connect"))
        a_auto.setCheckable(True)
        a_auto.setChecked(bool(rule.get("auto_connect")))
        a_auto.toggled.connect(lambda on: self._set_autoconnect(ref, dev, on))
        menu.addSeparator()
        menu.addAction(_("Properties...")).triggered.connect(self._open_properties)
        menu.addAction(_("Refresh")).triggered.connect(self._refresh)
        menu.exec(self.tree.viewport().mapToGlobal(pos))

    def _rule_for(self, ref) -> dict:
        """Fetch the current per-device rule (dict) from the daemon; empty dict if none."""
        if not ref or self.client is None:
            return {}
        mac, _dev = ref
        key = (self._selected_device() or {}).get("key", "")
        try:
            rules = self.client.get_settings().get("device_rules", [])
        except (ipc.IpcError, OSError):
            return {}
        for r in rules:
            if r.get("server_mac") == mac and r.get("key") == key:
                return r
        return {}

    def _set_autoconnect(self, ref, dev: dict, on: bool) -> None:
        if not ref:
            return
        mac, _dev_id = ref
        patch = {"mac": mac, "key": dev.get("key", ""),
                 "label": dev.get("description", ""), "auto_connect": on}
        threading.Thread(
            target=lambda: self._safe(lambda: self.client.set_device_rule(patch)),
            daemon=True).start()

    def _open_properties(self) -> None:
        dev = self._selected_device()
        srv = self._selected_server()
        if dev is None or srv is None:
            return
        rule = self._rule_for(self._selected_ref())
        dlg = DevicePropertiesDialog(dev, srv, rule, self)
        if dlg.exec():
            patch = dlg.result_rule()
            threading.Thread(
                target=lambda: self._safe(lambda: self.client.set_device_rule(patch)),
                daemon=True).start()

    def _settings(self) -> None:
        if self.client is None:
            return
        try:
            settings = self.client.get_settings()
        except (ipc.IpcError, OSError) as exc:
            QMessageBox.warning(self, _("Settings"), str(exc))
            return
        dlg = OptionsDialog(settings, self._servers, self)
        if dlg.exec():
            patch = dlg.result_patch()

            self._apply_settings_cache(patch)
            self._render(self._servers)      # apply filter/columns immediately

            def work():
                self._safe(lambda: self.client.set_settings(patch))
                # Bring the autostart entry in line with the (possibly changed) setting.
                try:
                    autostart.sync(bool(patch.get("autostart")),
                                   bool(patch.get("start_hidden")))
                except OSError:
                    pass

            threading.Thread(target=work, daemon=True).start()

    def _help(self) -> None:
        QMessageBox.information(
            self, _("Help"),
            _("Select a device in the tree and Connect/Disconnect - or use the "
              "right-click menu or the tray icon.\n\n"
              "If a device is held by another host, you can use \"Request use\"; the "
              "holding machine is asked.\n\n"
              "The background service keeps the connection even when this window is "
              "closed - the app then keeps running in the notification area."))

    def _about(self) -> None:
        QMessageBox.about(
            self, _("About Silex USB Device Server"),
            APP_TITLE + "\n\n"
            + _("Open replacement for SX Virtual Link for Silex USB device servers "
                "(USB-over-Ethernet).\n"
                "Uses USB/IP (vhci-hcd) and a local background service.")
            + f"\n\nOpenSilexLink {__version__}\n{PROJECT_URL}")


def _want_hidden(argv: list[str]) -> bool:
    """Start into the tray? Via --hidden (autostart) or the start_hidden setting.

    The setting is read directly from the JSON (no daemon needed) so the decision is
    known before building the window."""
    if "--hidden" in argv:
        return True
    try:
        return bool(SettingsStore().settings.start_hidden)
    except OSError:
        return False


def _install_translations(app, lang: str) -> None:
    """Load Qt's standard texts (Cancel/Yes/No ...) for the language. English is Qt's
    built-in default -> only German needs qtbase_de loaded."""
    if lang != "de":
        return
    from PySide6.QtCore import QLibraryInfo, QTranslator
    path = QLibraryInfo.path(QLibraryInfo.LibraryPath.TranslationsPath)
    tr = QTranslator(app)
    if tr.load("qtbase_de", path):
        app.installTranslator(tr)


def main() -> int:
    from PySide6.QtCore import QLocale
    app = QApplication(sys.argv)
    app.setApplicationName(APP_TITLE)
    # Language from the system locale (supports DE/EN; otherwise English).
    lang = i18n.set_language(QLocale.system().name())
    _install_translations(app, lang)
    # Closing the window minimizes to the tray -> do not quit the app when a tray exists.
    tray = QSystemTrayIcon.isSystemTrayAvailable()
    app.setQuitOnLastWindowClosed(not tray)
    win = MainWindow(address=os.environ.get("SILEX_IPC"))
    # start_hidden only when a tray exists - otherwise the app would be unreachable.
    if _want_hidden(sys.argv) and tray:
        win.hide()
    else:
        win.show()
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
