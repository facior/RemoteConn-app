# RemoteConn

Aplikacja do zdalnego pulpitu w stylu AnyDesk / AnyViewer: Rust + Tauri 2 (React + TypeScript), QUIC, szyfrowanie
end-to-end. Jedna aplikacja służy do udostępniania własnego komputera i do łączenia się z innymi.

Do łączenia przez internet po 9-cyfrowym ID potrzebny jest własny serwer ID z repozytorium **RemoteConn-server**.
Bez niego aplikacja łączy się po adresie IP, np. w sieci lokalnej.

## Funkcje

- **Łączenie po ID albo adresie IP.** Najpierw bezpośrednio (LAN, przejście przez NAT), a gdy się nie da, przez
  relay serwera. Pasek sesji pokazuje „P2P” albo „Relay”.
- **Bezpieczne hasła:** hasło jednorazowe albo stałe, okno akceptacji z wyborem uprawnień i zaufane urządzenia.
- **Sesja:** obraz H.264 z adaptacyjną jakością, wybór monitora, mysz i klawiatura, zdalny kursor, czat, wspólny
  schowek (tekst i obrazy), przesyłanie plików w obie strony ze wznawianiem, Ctrl+Alt+Del.
- **Konta:** zaloguj się na to samo konto na swoich komputerach, a połączysz się z nimi jednym kliknięciem, bez hasła
  („Moje komputery”). Zgubionemu urządzeniu odbierzesz dostęp.
- **Dostęp bez nadzoru (Windows):** usługa systemowa udostępnia komputer od jego włączenia, także na ekranie
  logowania, blokady i przy oknach UAC.
- **Linki `remoteconn://connect?id=…`:** np. przycisk „Połącz” w panelu serwera otwiera aplikację z wpisanym ID.
- **Ustawienia:** motyw, autostart, praca w zasobniku, jakość, folder plików, serwer i klucz dostępu, przypięty klucz
  serwera, konto i usługa.

| Platforma | Stan |
|---|---|
| Windows 10/11 | działa (udostępnianie i łączenie, usługa) |
| macOS | w planach (ScreenCaptureKit, VideoToolbox) |

## Pobieranie

Gotowe instalatory dla Windows są w **[Releases](https://github.com/facior/RemoteConn-app/releases)**
(`RemoteConn_*_x64-setup.exe` albo `.msi`). Program nie jest jeszcze podpisany certyfikatem, więc SmartScreen może
ostrzec przy pierwszym uruchomieniu: „Więcej informacji → Uruchom mimo to”.

Nowe wydanie: zmień wersję w `desktop/src-tauri/tauri.conf.json` (i `desktop/package.json`) i wypchnij tag, np.
`git tag v0.2.0 && git push origin v0.2.0`. Workflow `release.yml` zbuduje instalatory i opublikuje je.

## Jak używać

1. **Serwer:** Ustawienia → Sieć → adres serwera ID i klucz dostępu (od administratora), „Testuj”.
2. **Udostępnianie:** „Zezwól na połączenia”. Pojawią się ID i hasło jednorazowe; przekaż je osobie, która ma się
   połączyć.
3. **Łączenie:** wpisz ID (albo adres IP) i hasło z komputera zdalnego, „Połącz”. Druga strona akceptuje
   połączenie i wybiera, na co pozwala.
4. **Bez obecności drugiej osoby:**
   - stałe hasło (Ustawienia → Bezpieczeństwo), autostart i „Udostępniaj przy starcie”;
   - albo usługa (Ustawienia → Usługa → „Zainstaluj usługę”), która działa też przed zalogowaniem.
5. **Konto:** Ustawienia → Konto. Nowe konto zakłada się z kodem zaproszenia od administratora serwera. Po
   zalogowaniu na kilku komputerach widać je w „Moich komputerach” na ekranie głównym.

## Budowanie

Wymagania: [Rust](https://rustup.rs) stable (na Windows toolchain MSVC), Node.js 20+, WebView2 (jest w Windows 10/11).

```sh
cd desktop
npm install
npm run tauri dev                       # tryb deweloperski
npm run tauri build                     # instalator (MSI/NSIS)
npm run tauri build -- --no-bundle      # sam plik: target/release/remoteconn-desktop.exe
```

## Testy

```sh
cd desktop && npm run build && cd ..    # pierwszy raz: aplikacja Tauri potrzebuje zbudowanego frontendu
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Testy połączeń przez serwer ID (`crates/rc-session/tests/rendezvous.rs`) uruchamiają prawdziwy program `rc-server`.
Szukają go w `RC_SERVER_BIN` albo w sąsiednim katalogu `../RemoteConn-server/target/{release,debug}`; zbuduj go tam
przez `cargo build --release -p rc-server`. Bez serwera te testy są pomijane z komunikatem `SKIPPED`.

## Bezpieczeństwo w skrócie

- **Szyfrowanie:** każda sesja jest szyfrowana end-to-end (Noise XX nad QUIC), także przez relay.
- **Hasła:** SPAKE2 związane z sesją, więc hasło nigdy nie jest przesyłane. Blokada po błędnych próbach i
  spowolnienie przy atakach z wielu adresów.
- **Klucze na dysku:** klucz urządzenia, stałe hasło i sesja konta są szyfrowane przez Windows DPAPI.
- **Weryfikacja serwera:** certyfikat serwera jest przypinany przy pierwszym połączeniu. Przy łączeniu po ID
  aplikacja przyjmuje tylko klucz zarejestrowany dla tego ID.
- **Konta:**
  - klucz konta powstaje z hasła na urządzeniu, a urządzenia mają certyfikaty podpisane tym kluczem;
  - serwer zna tylko klucz publiczny, więc nie może nikogo wpuścić;
  - zmiana hasła i odebranie dostępu działają na hostach od razu.
- **Usługa:**
  - program usługi leży w Program Files;
  - zmiany ustawień usługi wymagają zgody administratora (UAC);
  - agent przyjmuje polecenia tylko z sesji zalogowanego użytkownika i otwiera pliki z jego uprawnieniami.
- **Dane od drugiej strony** (nazwy, czat, nazwy plików) są czyszczone ze znaków sterujących. Odebrane pliki dostają
  znacznik „Mark of the Web”.

Szczegóły: **[docs/APLIKACJA.md](docs/APLIKACJA.md)** (funkcje, architektura, model bezpieczeństwa, usługa, konta,
rozwiązywanie problemów).

## Struktura

| Ścieżka | Zawartość |
|---|---|
| `desktop/` | aplikacja Tauri: interfejs (`src/`, React) i backend (`src-tauri/`) |
| `desktop/src-tauri/src/service/` | usługa Windows, agent, instalacja, komunikacja okno–agent |
| `crates/rc-session` | logika hosta i viewera, konta po stronie hosta, transfer plików |
| `crates/rc-capture` | przechwytywanie ekranu i kursora (DXGI), przełączanie na ekran logowania / UAC |
| `crates/rc-codec` | koder H.264 (OpenH264) |
| `crates/rc-input` | mysz i klawiatura (SendInput) |
| `crates/rc-clipboard` | synchronizacja schowka |
| `crates/rc-proto` | protokoły sesji i serwera (protobuf) |
| `crates/rc-crypto` | tożsamość urządzenia, Noise, SPAKE2, klucze i certyfikaty kont |
| `crates/rc-transport` | QUIC, przejście przez NAT, klient relay, przypinanie certyfikatu |
| `crates/rc-rendezvous` | klient serwera ID: rejestracja, wyszukiwanie, konta |
| `scripts/sync-shared.ps1` | kopiuje wspólny protokół do repozytorium serwera |

### Protokół wspólny z serwerem

`rc-proto`, `rc-crypto`, `rc-transport` i `rc-rendezvous` są też w repozytorium serwera, żeby każde repozytorium
budowało się samodzielnie. Zmieniasz je **tutaj**, a potem:

```powershell
.\scripts\sync-shared.ps1          # kopiuje do ..\RemoteConn-server
.\scripts\sync-shared.ps1 -Check   # sprawdza, czy kopie są identyczne
```

Zatwierdź zmiany w obu repozytoriach.
