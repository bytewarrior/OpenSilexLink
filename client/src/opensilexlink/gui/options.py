"""Options dialog (4 tabs) - counterpart to SX Virtual Link Settings 1-4.

Loads the current settings (dict from the daemon) and returns, via
:meth:`result_patch`, the partial dict to save (to ``set_settings``).
"""

from __future__ import annotations

from PySide6.QtCore import Qt, QUrl
from PySide6.QtGui import QDesktopServices
from PySide6.QtWidgets import (
    QDialog, QTabWidget, QWidget, QVBoxLayout, QHBoxLayout, QFormLayout,
    QCheckBox, QSpinBox, QLineEdit, QPushButton, QListWidget, QListWidgetItem,
    QDialogButtonBox, QLabel, QGroupBox, QFileDialog,
)

from ..i18n import _
from .client import server_web_url


def _ro(text: str) -> QLabel:
    lbl = QLabel(text or "-")
    lbl.setTextInteractionFlags(Qt.TextSelectableByMouse)
    return lbl

# Display columns (key -> label), order = default
COLUMNS = [
    ("name", "Name"), ("type", "Type"), ("status", "Status"),
    ("hostname", "Hostname"), ("model", "Model"), ("ip", "IP address"),
    ("ipv6", "IPv6 address"), ("mac", "Ethernet address"),
]
_COL_LABEL = dict(COLUMNS)


class OptionsDialog(QDialog):
    def __init__(self, settings: dict, servers: list | None = None, parent=None):
        super().__init__(parent)
        self.setWindowTitle(_("Options"))
        self.resize(430, 470)
        self._settings = settings or {}
        self._servers = servers or []

        self.tabs = QTabWidget()
        self.tabs.addTab(self._tab_general(), _("General"))
        self.tabs.addTab(self._tab_search(), _("Search for device servers"))
        self.tabs.addTab(self._tab_display(), _("Display"))
        self.tabs.addTab(self._tab_filter(), _("Filter"))

        buttons = QDialogButtonBox(
            QDialogButtonBox.Ok | QDialogButtonBox.Cancel)
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)

        lay = QVBoxLayout(self)
        lay.addWidget(self.tabs, 1)
        lay.addWidget(buttons)

    # -- Tab 1: General ---------------------------------------------------- #
    def _tab_general(self) -> QWidget:
        s = self._settings
        w = QWidget()
        lay = QVBoxLayout(w)
        box = QGroupBox(_("Basic configuration"))
        form = QVBoxLayout(box)
        self.cb_autostart = QCheckBox(_("Start on system startup"))
        self.cb_start_hidden = QCheckBox(_("Do not show the main window at startup"))
        self.cb_notify = QCheckBox(_("Show a notification on error"))
        self.cb_confirm = QCheckBox(_("Show a confirmation when disconnecting USB devices"))
        self.cb_autoconn = QCheckBox(_("Automatically connect to newly detected USB devices"))
        self.cb_hide_close = QCheckBox(_("Hide the main window instead of quitting on close"))
        for cb, key in [(self.cb_autostart, "autostart"),
                        (self.cb_start_hidden, "start_hidden"),
                        (self.cb_notify, "notify_errors"),
                        (self.cb_confirm, "confirm_disconnect"),
                        (self.cb_autoconn, "auto_connect_new"),
                        (self.cb_hide_close, "hide_on_close")]:
            cb.setChecked(bool(s.get(key)))
            form.addWidget(cb)
        lay.addWidget(box)

        row = QHBoxLayout()
        row.addWidget(QLabel(_("Refresh interval (3-60 s):")))
        self.sp_refresh = QSpinBox()
        self.sp_refresh.setRange(3, 60)
        self.sp_refresh.setValue(int(s.get("refresh_interval", 10)))
        row.addWidget(self.sp_refresh)
        row.addStretch(1)
        lay.addLayout(row)
        lay.addStretch(1)
        return w

    # -- Tab 2: Search addresses ------------------------------------------- #
    def _tab_search(self) -> QWidget:
        s = self._settings
        w = QWidget()
        lay = QVBoxLayout(w)
        lay.addWidget(QLabel(_("For NAPT/routing cases where the broadcast does not "
                               "reach the device: explicit unicast addresses.")))
        self.cb_search = QCheckBox(_("Enable the following search addresses"))
        self.cb_search.setChecked(bool(s.get("search_enabled")))
        lay.addWidget(self.cb_search)

        row = QHBoxLayout()
        self.ed_addr = QLineEdit()
        self.ed_addr.setPlaceholderText(_("Address (IP)"))
        self.ed_port = QLineEdit()
        self.ed_port.setPlaceholderText(_("Port (optional)"))
        self.ed_port.setFixedWidth(90)
        btn_add = QPushButton(_("Add"))
        btn_add.clicked.connect(self._add_addr)
        row.addWidget(self.ed_addr, 1)
        row.addWidget(self.ed_port)
        row.addWidget(btn_add)
        lay.addLayout(row)

        self.lst_addr = QListWidget()
        for a in s.get("search_addresses", []):
            self._add_addr_item(a.get("address", ""), a.get("port"))
        lay.addWidget(self.lst_addr, 1)
        btn_del = QPushButton(_("Delete"))
        btn_del.clicked.connect(lambda: self._del_selected(self.lst_addr))
        lay.addWidget(btn_del)
        return w

    def _add_addr(self) -> None:
        addr = self.ed_addr.text().strip()
        if not addr:
            return
        port = self.ed_port.text().strip()
        self._add_addr_item(addr, int(port) if port.isdigit() else None)
        self.ed_addr.clear()
        self.ed_port.clear()

    def _add_addr_item(self, addr: str, port) -> None:
        text = f"{addr}:{port}" if port else addr
        item = QListWidgetItem(text)
        item.setData(Qt.UserRole, {"address": addr, "port": port})
        self.lst_addr.addItem(item)

    # -- Tab 3: Display ---------------------------------------------------- #
    def _tab_display(self) -> QWidget:
        s = self._settings
        w = QWidget()
        lay = QVBoxLayout(w)
        box = QGroupBox(_("List display"))
        outer = QVBoxLayout(box)
        outer.addWidget(QLabel(_("Choose the information to show in the USB device list.")))
        self._col_widths = dict(s.get("column_widths", {}))

        content = QHBoxLayout()
        self.lst_cols = QListWidget()
        enabled = list(s.get("columns", ["name", "type", "status", "hostname", "model"]))
        ordered = enabled + [k for k, _lbl in COLUMNS if k not in enabled]
        for key in ordered:
            item = QListWidgetItem(_(_COL_LABEL.get(key, key)))
            item.setData(Qt.UserRole, key)
            item.setFlags(item.flags() | Qt.ItemIsUserCheckable)
            item.setCheckState(Qt.Checked if key in enabled else Qt.Unchecked)
            self.lst_cols.addItem(item)
        self.lst_cols.currentItemChanged.connect(self._on_col_selected)
        content.addWidget(self.lst_cols, 1)

        btns = QVBoxLayout()
        for text, fn in [(_("Move up"), lambda: self._move(self.lst_cols, -1)),
                         (_("Move down"), lambda: self._move(self.lst_cols, +1)),
                         (_("Show"), lambda: self._set_col_checked(True)),
                         (_("Hide"), lambda: self._set_col_checked(False))]:
            b = QPushButton(text)
            b.clicked.connect(fn)
            btns.addWidget(b)
        btns.addStretch(1)
        content.addLayout(btns)
        outer.addLayout(content, 1)
        lay.addWidget(box, 1)

        row = QHBoxLayout()
        row.addWidget(QLabel(_("Width of the selected column:")))
        self.sp_colwidth = QSpinBox()
        self.sp_colwidth.setRange(20, 800)
        self.sp_colwidth.setEnabled(False)
        self.sp_colwidth.valueChanged.connect(self._on_colwidth_changed)
        row.addWidget(self.sp_colwidth)
        row.addStretch(1)
        lay.addLayout(row)
        self.lst_cols.setCurrentRow(0)
        return w

    def _default_col_width(self, key: str) -> int:
        return int(self._col_widths.get(key, 240 if key == "name" else 120))

    def _on_col_selected(self, current, previous) -> None:
        if previous is not None:                       # save the width of the previous column
            self._col_widths[previous.data(Qt.UserRole)] = self.sp_colwidth.value()
        if current is None:
            self.sp_colwidth.setEnabled(False)
            return
        self.sp_colwidth.setEnabled(True)
        self.sp_colwidth.blockSignals(True)            # no write-back while loading
        self.sp_colwidth.setValue(self._default_col_width(current.data(Qt.UserRole)))
        self.sp_colwidth.blockSignals(False)

    def _on_colwidth_changed(self, value: int) -> None:
        item = self.lst_cols.currentItem()
        if item is not None:
            self._col_widths[item.data(Qt.UserRole)] = value

    def _set_col_checked(self, checked: bool) -> None:
        item = self.lst_cols.currentItem()
        if item is not None:
            item.setCheckState(Qt.Checked if checked else Qt.Unchecked)

    # -- Tab 4: Filter ----------------------------------------------------- #
    def _tab_filter(self) -> QWidget:
        s = self._settings
        w = QWidget()
        lay = QVBoxLayout(w)
        self.cb_filter = QCheckBox(_("Enable filter - only show checked device servers"))
        self.cb_filter.setChecked(bool(s.get("filter_enabled")))
        lay.addWidget(self.cb_filter)
        self.lst_filter = QListWidget()
        allowed = set(s.get("filter_servers", []))
        seen = set()
        for srv in self._servers:
            mac = srv.get("mac", "")
            if not mac or mac in seen:
                continue
            seen.add(mac)
            label = f"{srv.get('model', '')}  {mac}  {srv.get('hostname', '')}".strip()
            item = QListWidgetItem(label)
            item.setData(Qt.UserRole, mac)
            item.setFlags(item.flags() | Qt.ItemIsUserCheckable)
            item.setCheckState(Qt.Checked if mac in allowed else Qt.Unchecked)
            self.lst_filter.addItem(item)
        lay.addWidget(self.lst_filter, 1)
        return w

    # -- shared helpers ---------------------------------------------------- #
    def _del_selected(self, lst: QListWidget) -> None:
        for item in lst.selectedItems():
            lst.takeItem(lst.row(item))

    def _move(self, lst: QListWidget, delta: int) -> None:
        row = lst.currentRow()
        new = row + delta
        if row < 0 or new < 0 or new >= lst.count():
            return
        item = lst.takeItem(row)
        lst.insertItem(new, item)
        lst.setCurrentRow(new)

    # -- result ------------------------------------------------------------ #
    def result_patch(self) -> dict:
        cur = self.lst_cols.currentItem()
        if cur is not None:                            # capture the current width value
            self._col_widths[cur.data(Qt.UserRole)] = self.sp_colwidth.value()
        cols = [self.lst_cols.item(i).data(Qt.UserRole)
                for i in range(self.lst_cols.count())
                if self.lst_cols.item(i).checkState() == Qt.Checked]
        addrs = [self.lst_addr.item(i).data(Qt.UserRole)
                 for i in range(self.lst_addr.count())]
        filt = [self.lst_filter.item(i).data(Qt.UserRole)
                for i in range(self.lst_filter.count())
                if self.lst_filter.item(i).checkState() == Qt.Checked]
        return {
            "autostart": self.cb_autostart.isChecked(),
            "start_hidden": self.cb_start_hidden.isChecked(),
            "notify_errors": self.cb_notify.isChecked(),
            "confirm_disconnect": self.cb_confirm.isChecked(),
            "auto_connect_new": self.cb_autoconn.isChecked(),
            "hide_on_close": self.cb_hide_close.isChecked(),
            "refresh_interval": self.sp_refresh.value(),
            "search_enabled": self.cb_search.isChecked(),
            "search_addresses": addrs,
            "columns": cols,
            "column_widths": self._col_widths,
            "filter_enabled": self.cb_filter.isChecked(),
            "filter_servers": filt,
        }


class DevicePropertiesDialog(QDialog):
    """Properties of a device (3 tabs like SX Virtual Link).

    ``device``/``server`` provide the display info, ``rule`` (dict or None) the
    stored per-device settings. :meth:`result_rule` returns the patch for
    ``set_device_rule``. Options that are not yet implemented are visible but
    disabled.
    """

    def __init__(self, device: dict, server: dict, rule: dict | None = None, parent=None):
        super().__init__(parent)
        self._device = device or {}
        self._server = server or {}
        self._rule = rule or {}
        self.setWindowTitle(
            _("Properties of {}").format(self._device.get("description", _("device"))))
        self.resize(430, 460)

        tabs = QTabWidget()
        tabs.addTab(self._tab_general(), _("General"))
        tabs.addTab(self._tab_connect(), _("Connect"))
        tabs.addTab(self._tab_disconnect(), _("Disconnect"))
        buttons = QDialogButtonBox(QDialogButtonBox.Ok | QDialogButtonBox.Cancel)
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        lay = QVBoxLayout(self)
        lay.addWidget(tabs, 1)
        lay.addWidget(buttons)

    def _tab_general(self) -> QWidget:
        d, s = self._device, self._server
        w = QWidget()
        outer = QVBoxLayout(w)
        outer.addWidget(QLabel(_("Name:")))
        self.ed_alias = QLineEdit(self._rule.get("alias") or d.get("description", ""))
        outer.addWidget(self.ed_alias)
        box = QGroupBox(_("Details"))
        form = QFormLayout(box)
        vp = f"{d.get('id_vendor', 0):04x}:{d.get('id_product', 0):04x}"
        form.addRow(_("USB device name:"), _ro(d.get("description", "")))
        form.addRow(_("VendorID:ProductID:"), _ro(vp))
        form.addRow(_("Transfer speed:"), _ro(d.get("speed", "")))
        form.addRow(_("Model:"), _ro(s.get("model", "")))
        form.addRow(_("Hostname:"), _ro(s.get("hostname", "")))
        form.addRow(_("IP address:"), _ro(s.get("ip", "")))
        form.addRow(_("Ethernet address:"), _ro((s.get("mac", "") or "").upper()))
        outer.addWidget(box)

        # Opens the device server's web interface in the browser (like SX Virtual Link).
        row = QHBoxLayout()
        row.addStretch(1)
        self.btn_server = QPushButton(_("Device server properties"))
        self.btn_server.setEnabled(bool(server_web_url(s)))
        self.btn_server.clicked.connect(self._open_server_web)
        row.addWidget(self.btn_server)
        outer.addLayout(row)
        outer.addStretch(1)
        return w

    def _open_server_web(self) -> None:
        url = server_web_url(self._server)
        if url:
            QDesktopServices.openUrl(QUrl(url))

    def _tab_connect(self) -> QWidget:
        r = self._rule
        w = QWidget()
        lay = QVBoxLayout(w)
        box = QGroupBox(_("Auto connect"))
        v = QVBoxLayout(box)
        self.cb_auto = QCheckBox(_("Connect this device automatically when available"))
        self.cb_auto.setChecked(bool(r.get("auto_connect")))
        self.cb_reconnect = QCheckBox(_("Reconnect automatically after a connection error"))
        self.cb_reconnect.setChecked(bool(r.get("reconnect_on_error", True)))
        v.addWidget(self.cb_auto)
        v.addWidget(self.cb_reconnect)
        lay.addWidget(box)

        appbox = QGroupBox(_("Launch an application when connected"))
        av = QVBoxLayout(appbox)
        row = QHBoxLayout()
        self.ed_app = QLineEdit(r.get("launch_app", ""))
        self.ed_app.setPlaceholderText(_("Path to the application (optionally with arguments)"))
        btn_browse = QPushButton(_("Browse..."))
        btn_browse.clicked.connect(self._browse_app)
        row.addWidget(self.ed_app, 1)
        row.addWidget(btn_browse)
        av.addLayout(row)
        self.cb_app_close = QCheckBox(
            _("Disconnect automatically when the application closes"))
        self.cb_app_close.setChecked(bool(r.get("disconnect_on_app_close")))
        av.addWidget(self.cb_app_close)
        lay.addWidget(appbox)
        lay.addStretch(1)
        return w

    def _browse_app(self) -> None:
        path, _sel = QFileDialog.getOpenFileName(self, _("Select application"))
        if path:
            self.ed_app.setText(path)

    def _tab_disconnect(self) -> QWidget:
        r = self._rule
        w = QWidget()
        lay = QVBoxLayout(w)
        self.cb_auto_disc = QCheckBox(
            _("Allow automatic disconnect when use is requested"))
        self.cb_auto_disc.setChecked(bool(r.get("auto_disconnect_on_request")))
        lay.addWidget(self.cb_auto_disc)
        lay.addWidget(QLabel(
            _("If enabled, the connection is dropped - even while in use - as soon as\n"
              "another machine requests the device. The timeout is the grace period\n"
              "after which it is released automatically (0 = immediately).")))
        row = QHBoxLayout()
        row.addWidget(QLabel(_("Timeout for automatic disconnect (s):")))
        self.sp_timeout = QSpinBox()
        self.sp_timeout.setRange(0, 3600)
        self.sp_timeout.setValue(int(r.get("auto_disconnect_timeout", 120)))
        row.addWidget(self.sp_timeout)
        row.addStretch(1)
        lay.addLayout(row)
        lay.addStretch(1)
        # The timeout is only editable when auto-disconnect is enabled.
        self.sp_timeout.setEnabled(self.cb_auto_disc.isChecked())
        self.cb_auto_disc.toggled.connect(self.sp_timeout.setEnabled)
        return w

    def result_rule(self) -> dict:
        """Patch for set_device_rule (incl. mac/key)."""
        return {
            "mac": self._server.get("mac", ""),
            "key": self._device.get("key", ""),
            "alias": self.ed_alias.text().strip(),
            "auto_connect": self.cb_auto.isChecked(),
            "reconnect_on_error": self.cb_reconnect.isChecked(),
            "launch_app": self.ed_app.text().strip(),
            "disconnect_on_app_close": self.cb_app_close.isChecked(),
            "auto_disconnect_on_request": self.cb_auto_disc.isChecked(),
            "auto_disconnect_timeout": self.sp_timeout.value(),
        }
