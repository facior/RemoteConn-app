import { Channel, invoke } from "@tauri-apps/api/core";

export interface DeviceInfo {
  name: string;
  fingerprint: string;
  version: string;
}

export interface HostStatus {
  addresses: string[];
  port: number;
  password: string;
  usesRendezvous: boolean;
  /** Last known ID ("123 456 789"), empty before the first registration. */
  deviceId: string;
  permanentPasswordSet: boolean;
  /** Sharing runs in the background service. */
  service: boolean;
  /** Version of the program that shares (service may lag behind the app). */
  version: string;
}

export interface RendezvousEvent {
  online: boolean;
  deviceId: string;
  reason: string;
}

export type Theme = "system" | "light" | "dark";

export interface DefaultPermissions {
  viewScreen: boolean;
  controlInput: boolean;
  clipboard: boolean;
  fileTransfer: boolean;
}

export interface TrustedDevice {
  fingerprint: string;
  name: string;
  addedUnix: number;
}

/** Mirrors `config::Settings` in the backend. */
export interface Settings {
  deviceName: string;
  theme: Theme;
  autostart: boolean;
  closeToTray: boolean;
  shareOnStart: boolean;
  passwordLength: number;
  rotatePasswordAfterSession: boolean;
  confirmPermanentPassword: boolean;
  defaultPermissions: DefaultPermissions;
  trustedDevices: TrustedDevice[];
  defaultQuality: QualityPreset;
  defaultFps: number;
  downloadDir: string;
  showRemoteCursor: boolean;
  soundNotifications: boolean;
  rendezvousServer: string;
  serverAccessKey: string;
  listenPort: number;
  forceRelay: boolean;
}

export interface SettingsView {
  settings: Settings;
  permanentPasswordSet: boolean;
  computerName: string;
  defaultDownloadDir: string;
  /** Pinned certificate of the configured server (short form). */
  serverFingerprint: string | null;
}

export interface ServerTest {
  latencyMs: number;
  fingerprint: string;
  newlyPinned: boolean;
}

/** Server settings read from an administrator's configuration code. */
export interface ImportedServer {
  server: string;
  accessKey: string;
  fingerprint: string;
}

export interface AutoAccepted {
  peer: Peer;
  reason: "permanentPassword" | "trustedDevice" | "account";
}

export interface Peer {
  /** Host side: the session this peer belongs to. */
  sessionId: number;
  deviceName: string;
  fingerprint: string;
  addr: string;
  /** Traffic goes through the relay server instead of peer to peer. */
  relayed: boolean;
  /** Host side: how the viewer got in. */
  auth: "oneTime" | "permanent" | "account";
  /** Host side, with account access: the account name. */
  account: string | null;
}

export interface GrantedPermissions {
  viewScreen: boolean;
  controlInput: boolean;
  clipboard: boolean;
  fileTransfer: boolean;
}

export interface HostChat {
  sessionId: number;
  text: string;
}

export type QualityPreset = "balanced" | "quality" | "speed";

export interface Transfer {
  /** Decimal string (u64 does not fit a JS number). */
  id: string;
  name: string;
  incoming: boolean;
  transferred: number;
  total: number;
  state: "preparing" | "running" | "done" | "failed" | "cancelled";
  path: string | null;
  error: string | null;
}

export interface HostTransfer {
  sessionId: number;
  transfer: Transfer;
}

export interface ChatLine {
  mine: boolean;
  text: string;
  at: number;
}

export interface IncomingRequest {
  id: number;
  peer: Peer;
  /** Initial state of the permission checkboxes. */
  permissions: DefaultPermissions;
}

export type HostSessionEvent =
  | { state: "started"; peer: Peer }
  | { state: "ended"; peer: Peer; reason: string };

export interface Monitor {
  id: number;
  name: string;
  width: number;
  height: number;
  primary: boolean;
}

export type SessionEvent =
  | { type: "videoConfig"; codec: string; monitorId: number; width: number; height: number; fps: number; bitrateKbps: number }
  | { type: "cursorShape"; url: string; width: number; height: number; hotX: number; hotY: number }
  | { type: "cursorPosition"; monitorId: number; x: number; y: number; visible: boolean }
  | { type: "monitors"; monitors: Monitor[] }
  | { type: "permissions"; viewScreen: boolean; controlInput: boolean; clipboard: boolean; fileTransfer: boolean }
  | { type: "chat"; text: string; sentUnixMs: number }
  | { type: "transfer"; transfer: Transfer }
  | { type: "stats"; rttMs: number; lossPermille: number; receivedKbps: number; fps: number }
  | { type: "closed"; reason: string };

export interface VideoFrame {
  keyframe: boolean;
  monitorId: number;
  frameId: bigint;
  timestampUs: number;
  data: Uint8Array;
}

/** Parses the 24-byte header written by `commands::frame_message`. */
export function parseFrame(buf: ArrayBuffer): VideoFrame | null {
  if (buf.byteLength < 24) return null;
  const view = new DataView(buf);
  if (view.getUint8(0) !== 1) return null;
  return {
    keyframe: view.getUint8(1) === 1,
    monitorId: view.getUint32(4, true),
    frameId: view.getBigUint64(8, true),
    timestampUs: Number(view.getBigUint64(16, true)),
    data: new Uint8Array(buf, 24),
  };
}

export type MouseEventKind = "move" | "down" | "up" | "wheel";

export interface RemoteMouse {
  kind: MouseEventKind;
  button?: number;
  monitorId?: number;
  x?: number;
  y?: number;
  wheelDx?: number;
  wheelDy?: number;
}

export interface AccountView {
  loggedIn: boolean;
  name: string;
  displayName: string;
  server: string;
  hostTrust: boolean;
  disabled: boolean;
}

export interface AccountDevice {
  /** Formatted ID; empty for devices that never shared their screen. */
  deviceId: string;
  name: string;
  online: boolean;
  publicKey: string;
  fingerprint: string;
  thisDevice: boolean;
  addedUnix: number;
}

export interface ServiceStatus {
  supported: boolean;
  installed: boolean;
  running: boolean;
  connected: boolean;
}

export const api = {
  deviceInfo: () => invoke<DeviceInfo>("device_info"),
  takePendingLink: () => invoke<string | null>("take_pending_link"),
  getSettings: () => invoke<SettingsView>("get_settings"),
  setSettings: (settings: Settings) => invoke<SettingsView>("set_settings", { settings }),
  setPermanentPassword: (password: string | null) => invoke<SettingsView>("set_permanent_password", { password }),
  removeTrustedDevice: (fingerprint: string) => invoke<SettingsView>("remove_trusted_device", { fingerprint }),
  testServer: (server: string, accessKey: string) => invoke<ServerTest>("test_server", { server, accessKey }),
  forgetServerPin: (server: string) => invoke<SettingsView>("forget_server_pin", { server }),
  importConfigCode: (code: string) => invoke<ImportedServer>("import_config_code", { code }),
  autostartEnabled: () => invoke<boolean>("autostart_enabled"),
  hostStart: () => invoke<HostStatus>("host_start"),
  hostStatus: () => invoke<HostStatus | null>("host_status"),
  hostStop: () => invoke<void>("host_stop"),
  hostNewPassword: () => invoke<string>("host_new_password"),
  hostRespond: (id: number, accept: boolean, permissions: GrantedPermissions, trust: boolean) =>
    invoke<void>("host_respond", { id, accept, permissions, trust }),
  hostSendChat: (sessionId: number, text: string) => invoke<void>("host_send_chat", { sessionId, text }),
  hostDisconnect: (sessionId: number) => invoke<void>("host_disconnect", { sessionId }),
  viewerSendChat: (text: string) => invoke<void>("viewer_send_chat", { text }),
  hostSendFiles: (sessionId: number, paths: string[]) => invoke<void>("host_send_files", { sessionId, paths }),
  hostCancelTransfer: (sessionId: number, id: string) => invoke<void>("host_cancel_transfer", { sessionId, id }),
  viewerSendFiles: (paths: string[]) => invoke<void>("viewer_send_files", { paths }),
  viewerCancelTransfer: (id: string) => invoke<void>("viewer_cancel_transfer", { id }),
  revealPath: (path: string) => invoke<void>("reveal_path", { path }),
  viewerSetQuality: (preset: QualityPreset, maxFps: number) => invoke<void>("viewer_set_quality", { preset, maxFps }),
  viewerConnect: (
    address: string,
    password: string,
    frames: Channel<ArrayBuffer>,
    events: Channel<SessionEvent>,
  ) => invoke<{ peer: Peer }>("viewer_connect", { address, password, frames, events }),
  viewerDisconnect: () => invoke<void>("viewer_disconnect"),
  viewerMouse: (event: RemoteMouse) => invoke<void>("viewer_mouse", { event }),
  viewerKey: (hid: number, down: boolean) => invoke<void>("viewer_key", { hid, down }),
  viewerSelectMonitor: (id: number) => invoke<void>("viewer_select_monitor", { id }),
  viewerRequestKeyframe: () => invoke<void>("viewer_request_keyframe"),
  viewerSpecialKey: (kind: "ctrlAltDel") => invoke<void>("viewer_special_key", { kind }),
  accountStatus: () => invoke<AccountView>("account_status"),
  accountRegister: (name: string, password: string, invite: string, displayName: string) =>
    invoke<AccountView>("account_register", { name, password, invite, displayName }),
  accountLogin: (name: string, password: string) => invoke<AccountView>("account_login", { name, password }),
  accountLogout: () => invoke<AccountView>("account_logout"),
  accountDevices: () => invoke<AccountDevice[]>("account_devices"),
  accountRevoke: (publicKey: string) => invoke<void>("account_revoke", { publicKey }),
  accountChangePassword: (oldPassword: string, newPassword: string) =>
    invoke<AccountView>("account_change_password", { oldPassword, newPassword }),
  accountSetHostTrust: (enabled: boolean) => invoke<AccountView>("account_set_host_trust", { enabled }),
  serviceStatus: () => invoke<ServiceStatus>("service_status"),
  serviceInstall: () => invoke<ServiceStatus>("service_install"),
  serviceUninstall: () => invoke<ServiceStatus>("service_uninstall"),
  serviceSync: () => invoke<void>("service_sync"),
};
