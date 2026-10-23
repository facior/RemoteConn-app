// KeyboardEvent.code (physical key) -> USB HID usage (page 0x07).
// The host maps HID usages to its own scancodes, so its layout decides the
// character, exactly like a locally attached keyboard.

const map: Record<string, number> = {
  Enter: 0x28, Escape: 0x29, Backspace: 0x2a, Tab: 0x2b, Space: 0x2c,
  Minus: 0x2d, Equal: 0x2e, BracketLeft: 0x2f, BracketRight: 0x30, Backslash: 0x31,
  Semicolon: 0x33, Quote: 0x34, Backquote: 0x35, Comma: 0x36, Period: 0x37, Slash: 0x38,
  CapsLock: 0x39, PrintScreen: 0x46, ScrollLock: 0x47, Pause: 0x48,
  Insert: 0x49, Home: 0x4a, PageUp: 0x4b, Delete: 0x4c, End: 0x4d, PageDown: 0x4e,
  ArrowRight: 0x4f, ArrowLeft: 0x50, ArrowDown: 0x51, ArrowUp: 0x52,
  NumLock: 0x53, NumpadDivide: 0x54, NumpadMultiply: 0x55, NumpadSubtract: 0x56,
  NumpadAdd: 0x57, NumpadEnter: 0x58, NumpadDecimal: 0x63,
  IntlBackslash: 0x64, ContextMenu: 0x65,
  ControlLeft: 0xe0, ShiftLeft: 0xe1, AltLeft: 0xe2, MetaLeft: 0xe3,
  ControlRight: 0xe4, ShiftRight: 0xe5, AltRight: 0xe6, MetaRight: 0xe7,
};

for (let i = 0; i < 26; i++) map[`Key${String.fromCharCode(65 + i)}`] = 0x04 + i;
for (let i = 1; i <= 9; i++) map[`Digit${i}`] = 0x1e + i - 1;
map.Digit0 = 0x27;
for (let i = 1; i <= 12; i++) map[`F${i}`] = 0x3a + i - 1;
for (let i = 1; i <= 9; i++) map[`Numpad${i}`] = 0x59 + i - 1;
map.Numpad0 = 0x62;

export function hidUsage(code: string): number | undefined {
  return map[code];
}
