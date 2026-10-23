# RemoteConn: aplikacja, bezpieczeństwo, usługa i konta

Opis aplikacji desktopowej. Serwer ID (rendezvous, relay, panel WWW, konta) jest w osobnym repozytorium
(RemoteConn-server) razem z instrukcją instalacji.

---

## 1. Co to jest

RemoteConn to aplikacja do zdalnego pulpitu w stylu AnyDesk/AnyViewer. Jedna aplikacja (Windows, Tauri) służy
zarówno do udostępniania własnego komputera, jak i do łączenia się z innymi. Opcjonalny serwer ID na Linuxie nadaje
komputerom 9-cyfrowe numery ID i pomaga łączyć się przez internet.

| Etap | Zakres | Stan |
|---|---|---|
| 0–1 | Fundament, pulpit w sieci lokalnej (Windows) | gotowe |
| 2 | Serwer ID, przechodzenie przez NAT, relay | gotowe |
| Funkcje | Czat, schowek, pliki, zdalny kursor, jakość | gotowe |
| Ustawienia | Ekran ustawień, stałe hasło, zaufane urządzenia | gotowe |
| Bezpieczeństwo | Przegląd i poprawki | gotowe |
| Panel WWW (A) | Panel administracyjny serwera, „Połącz” otwiera aplikację | gotowe |
| Panel WWW (B) | Łączenie z komputerami prosto z przeglądarki | do zrobienia |
| 3 | macOS | do zrobienia |
| 5 | Dostęp bez nadzoru: usługa Windows, ekran logowania, UAC, Ctrl+Alt+Del (rozdz. 6) | gotowe |
| 6 | Konta: łączenie bez hasła między własnymi komputerami, „Moje komputery” (rozdz. 7) | gotowe |
| 7 | Instalatory, podpis kodu, auto-aktualizacje | do zrobienia |

## 2. Funkcje aplikacji

- **Udostępnianie ekranu:** ID (z serwerem) albo adres IP, hasło jednorazowe, okno akceptacji z wyborem uprawnień
  (podgląd, sterowanie, schowek, pliki).
- **Łączenie:** po ID albo adresie IP. Najpierw połączenie bezpośrednie (LAN, hole punching), a jeśli się nie uda,
  automatycznie relay. Pasek sesji pokazuje „P2P” albo „Relay”.
- **Obraz:** H.264, dekodowanie sprzętowe w aplikacji (WebCodecs), wybór monitora, adaptacyjny bitrate, wybór
  jakości (zrównoważona, najlepsza, najszybsza) i FPS (15/30/60), statystyki (opóźnienie, straty, przepływność).
- **Sterowanie:** mysz i klawiatura według fizycznych klawiszy, więc znaki zależą od układu klawiatury komputera
  zdalnego. Kształt zdalnego kursora staje się lokalnym kursorem.
- **Czat** w obie strony. Host widzi listę aktywnych sesji i może każdą rozłączyć.
- **Schowek:** tekst i obrazy w obie strony.
- **Pliki** w obie strony: przeciągnij na okno albo „Wyślij pliki”. Przerwany transfer wznawia się od miejsca
  przerwania, po odbiorze plik jest weryfikowany sumą kontrolną BLAKE3 i trafia do `Pobrane\RemoteConn`.
- **Ustawienia:**
  - Ogólne: nazwa urządzenia, motyw, autostart, udostępnianie przy starcie, zamykanie do zasobnika;
  - Bezpieczeństwo: stałe hasło, długość i rotacja hasła jednorazowego, domyślne uprawnienia, zaufane urządzenia;
  - Sesja: domyślna jakość i FPS, folder na pliki, zdalny kursor, dźwięki;
  - Sieć: kod konfiguracji od administratora (`RC1-…`, „Wklej kod”: adres, klucz i zaufany klucz serwera naraz),
    serwer ID z testem połączenia, klucz dostępu, przypięty klucz serwera, port, wymuszenie relay;
  - Konto: logowanie, zakładanie konta z kodem zaproszenia, urządzenia na koncie, zmiana hasła (rozdz. 7);
  - Usługa: instalacja usługi systemowej do dostępu bez nadzoru (rozdz. 6).
- **Ctrl+Alt+Del** na komputerze zdalnym (przycisk w pasku sesji; działa, gdy tam jest zainstalowana usługa).
- **Linki `remoteconn://connect?id=…`** (np. przycisk „Połącz” w panelu serwera) wpisują ID w formularz; hasło
  podaje się ręcznie (rozdz. 5).

Zamknięcie okna chowa aplikację do zasobnika, a udostępnianie działa dalej. Aplikację kończy się z menu ikony
w zasobniku („Zakończ”).

## 3. Architektura

```
 [Viewer]  ⇄  QUIC + szyfrowanie end-to-end (Noise)  ⇄  [Host]
    │      bezpośrednio (P2P)  albo  przez relay (sam szyfrogram)
    └────────────── serwer ID: rejestr ID, sygnalizacja NAT, relay ──────────────┘
```

| Element | Rola |
|---|---|
| `crates/rc-proto` | protokoły sesji i serwera (protobuf), czyszczenie tekstów od drugiej strony |
| `crates/rc-crypto` | tożsamość urządzenia, kanał Noise z ochroną przed powtórzeniem, SPAKE2 |
| `crates/rc-transport` | QUIC (quinn), wyścig połączeń, hole punching, klient relay, przypinanie certyfikatu serwera |
| `crates/rc-rendezvous` | klient serwera ID (rejestracja hosta, wyszukiwanie po ID) |
| `crates/rc-capture` | przechwytywanie ekranu i kursora (DXGI) |
| `crates/rc-codec` | koder H.264 (OpenH264), zmiana bitrate'u w locie |
| `crates/rc-input` | wstrzykiwanie myszy i klawiatury (SendInput) |
| `crates/rc-clipboard` | synchronizacja schowka |
| `crates/rc-session` | logika hosta i viewera, konta po stronie hosta, transfer plików, regulator bitrate'u |
| `desktop` | aplikacja Tauri (React + TypeScript), usługa Windows i agent (`desktop/src-tauri/src/service`) |

Cztery pierwsze biblioteki (protokół) są też w repozytorium serwera; zmienia się je tutaj i kopiuje skryptem
`scripts/sync-shared.ps1`.

Każda sesja używa dwóch zaszyfrowanych strumieni (sterowanie oraz osobny dla plików, żeby duży plik nie opóźniał
myszy) i datagramów dla obrazu.

## 4. Bezpieczeństwo

### 4.1 Model

- **Tożsamość urządzenia:** klucze X25519 (szyfrowanie Noise) i Ed25519 (podpis rejestracji ID).
  Na Windows są zaszyfrowane przez DPAPI i powiązane z kontem użytkownika.
- **Szyfrowanie:** każda sesja to Noise XX end-to-end, także przez relay.
- **Hasło:** SPAKE2 związane z sesją Noise, więc hasło nigdy nie jest przesyłane, a atakujący pośrodku nie może
  skleić dwóch sesji.
- **Stałe hasło:** zaszyfrowane przez DPAPI. Działa obok hasła jednorazowego: viewer prowadzi równolegle dwie
  wymiany SPAKE2 z tym samym hasłem, a host dopasowuje je do obu haseł.
- **Weryfikacja hosta:** przy łączeniu po ID aplikacja akceptuje tylko klucz zarejestrowany dla tego ID.
- **Weryfikacja serwera:** certyfikat serwera jest przypinany przy pierwszym połączeniu (TOFU). Zmiana oznacza
  odmowę połączenia i czytelny komunikat.
- **Zaufane urządzenia:** rozpoznawane po pełnym kluczu publicznym (256 bitów); hasło jest nadal wymagane.

### 4.2 Błędy znalezione w przeglądzie bezpieczeństwa i naprawione

| # | Problem | Naprawa |
|---|---|---|
| 1 | Równoległe połączenia omijały blokadę zgadywania hasła | jedna próba naraz z danego IP; spowolnienie przy atakach z wielu adresów; test potwierdza ≤ 3 sprawdzone hasła z 10 równoczesnych prób |
| 2 | Zamaskowane rozszerzenia plików (`zdjecie\u202Egpj.exe` wygląda jak obrazek) | usuwanie znaków kierunku tekstu i sterujących z nazw plików, nazw urządzeń i czatu |
| 3 | Odebrane pliki uruchamiały się bez ostrzeżenia | znacznik „Mark of the Web”, więc Windows ostrzega przed uruchomieniem |
| 4 | Obcy mógł zmusić hosta do wysyłania pakietów w jego sieć LAN i poznać jego adresy wewnętrzne | adresy LAN wymieniane tylko za tym samym publicznym IP (filtr na serwerze i u hosta) |
| 5 | Serwer otwarty dla każdego, bez limitów | klucz dostępu, weryfikacja adresów QUIC Retry, limity połączeń (IPv6 per /64), limit nowych ID (10/h), limit sesji relay |
| 6 | Wielokrotne zamawianie relaya zalewało hosta | relay zamawiany raz na sesję |
| 7 | Aplikacja nie weryfikowała serwera | trwały certyfikat serwera i jego przypinanie (TOFU) w aplikacji |
| 8 | Host przed uwierzytelnieniem zdradzał nazwę, ID i to, czy ma stałe hasło | te dane wysyłane dopiero po uwierzytelnieniu |
| 9 | Brak limitu połączeń przed uwierzytelnieniem; mapa nieudanych prób rosła bez końca | limit 16; sprzątanie starych wpisów |
| 10 | Zaufane urządzenia rozpoznawane po 64-bitowym skrócie | pełny klucz 256-bitowy (**wcześniej dodane trzeba dodać ponownie**) |
| 11 | Viewer mógł zalewać hosta żądaniami klatek kluczowych i zmian monitora | dławienie żądań |
| 12 | Możliwe wyczerpanie pamięci złośliwymi danymi | walidacja kształtu kursora, limit bufora klatek, limit obrazów schowka, max 8 plików naraz, max 8 strumieni na połączenie |
| 13 | Klucz prywatny serwera zapisywany jako czytelny dla wszystkich (Linux) | tworzony z uprawnieniami 0600 |

Wcześniejsze poprawki z testów: zmiana długości hasła nie działała od razu, ekran ustawień pokazywał nieaktualną listę
zaufanych urządzeń, serwer relay przejmował strumień plików.

### 4.3 Do świadomości

- Klucz dostępu do serwera jest w `config.json` aplikacji bez szyfrowania. To klucz do serwera, nie do komputerów.
- Viewer automatycznie przyjmuje pliki od hosta, z którym sam się połączył (z ostrzeżeniem „Mark of the Web”).
- Kursory odwracające kolory (np. kursor tekstowy na ciemnym tle) są rysowane na czarno.
- Na macOS klucze nie są jeszcze szyfrowane (Keychain dojdzie w etapie macOS).

## 5. Linki `remoteconn://` w aplikacji

Aplikacja przy starcie rejestruje (dla bieżącego użytkownika Windows, bez uprawnień administratora) obsługę linków
`remoteconn://connect?id=123456789`. Link tylko wpisuje ID w formularz „Połącz z komputerem” i ustawia kursor w polu
hasła: nigdy nie łączy się sam i nie zmienia ustawień, więc złośliwy link nie może zrobić nic więcej niż wpisane
ręcznie ID. Gdy aplikacja już działa, nowy proces przekazuje ID do działającej instancji (lokalnie, port TCP 21119 na
127.0.0.1) i od razu się kończy.

**Ważne:** nie usuwaj `/var/lib/remoteconn/server.key` ani `server.crt`. Aplikacje mają przypięty certyfikat, a nowy
spowoduje ostrzeżenie o podszyciu się pod serwer. Jeśli zmiana jest celowa (reinstalacja), w aplikacji użyj
„Zapomnij klucz serwera” w Ustawienia → Sieć.

## 6. Dostęp bez nadzoru (usługa Windows)

Bez usługi komputer jest dostępny tylko wtedy, gdy ktoś jest zalogowany i aplikacja działa. Usługa usuwa to
ograniczenie: komputer jest dostępny od włączenia, także na ekranie logowania, na ekranie blokady i przy oknach UAC.

**Instalacja:** Ustawienia → Usługa → „Zainstaluj usługę” i zgoda administratora (UAC). Instalator:
1. kopiuje program do `C:\Program Files\RemoteConn\` (usługa działa jako SYSTEM, więc nie może uruchamiać pliku, który
   zwykły użytkownik mógłby podmienić);
2. tworzy `C:\ProgramData\RemoteConn\service\` z dostępem tylko dla SYSTEM i Administratorów;
3. rejestruje usługę „RemoteConn — dostęp bez nadzoru” (start automatyczny, restart po awarii);
4. włącza zasadę „SoftwareSASGeneration”, jeśli nie była ustawiona, żeby działało Ctrl+Alt+Del; przy odinstalowaniu
   ją cofa;
5. dodaje regułę zapory dla połączeń UDP do programu usługi;
6. przekazuje usłudze ustawienia, klucz urządzenia (to samo ID) i stałe hasło z aplikacji.

**Jak działa:**
- Usługa (konto SYSTEM, sesja 0) uruchamia w aktywnej sesji konsoli **agenta**, który udostępnia ekran. Zmiana
  użytkownika albo awaria agenta oznacza jego ponowne uruchomienie.
- Agent przełącza się na pulpit logowania i UAC, więc widać je i można nimi sterować.
- Okno aplikacji, gdy działa, łączy się z agentem przez lokalny potok i pokazuje to samo co zwykle: ID, hasło, okno
  akceptacji, czat, pliki. Bez okna (np. przed zalogowaniem) agent przyjmuje tylko połączenia, które nie wymagają
  akceptacji: stałe hasło (bez opcji „Pytaj o zgodę”), zaufane urządzenia i urządzenia konta.
- „Ctrl+Alt+Del” w pasku sesji agent przekazuje usłudze, a ta wywołuje `SendSAS`.

**Zabezpieczenia usługi:**

| Ryzyko | Ochrona |
|---|---|
| Podmiana programu działającego jako SYSTEM | program w Program Files (zapis tylko dla administratorów) |
| Zmiana ustawień usługi przez zwykłego użytkownika | ustawienia i stałe hasło trafiają do usługi tylko przez proces z uprawnieniami administratora (UAC); potok usługi dostępny tylko dla SYSTEM i Administratorów |
| Inny użytkownik sterujący agentem | potok agenta przyjmuje tylko programy z tej samej sesji Windows |
| Wyciągnięcie plików systemowych przez „Wyślij plik” | agent otwiera wysyłane pliki z uprawnieniami zalogowanego użytkownika, nie konta SYSTEM |
| Pliki od drugiej strony bez zalogowanego użytkownika | odbieranie plików jest wtedy wyłączone; po zalogowaniu trafiają do `Pobrane\RemoteConn` użytkownika |
| Odczyt sekretów usługi | DPAPI konta SYSTEM; katalog danych tylko dla SYSTEM i Administratorów |

**Zmiana ustawień:** ustawienia udostępniania zmienia się w aplikacji jak zwykle. Po zmianie serwera, stałego hasła,
uprawnień, zaufanych urządzeń albo konta aplikacja sama przekazuje je usłudze, prosząc o zgodę administratora (UAC).
Ręcznie: Ustawienia → Usługa → „Przekaż ustawienia do usługi”.

**Odinstalowanie:** Ustawienia → Usługa → „Odinstaluj usługę” (UAC). Usuwa usługę, regułę zapory, zasadę Ctrl+Alt+Del
(jeśli ustawił ją instalator), program z Program Files i dane usługi.

## 7. Konta (łączenie bez hasła między własnymi komputerami)

Jak w AnyViewer: logujesz się na to samo konto na swoich komputerach, w „Moich komputerach” widzisz, które są online,
i łączysz się jednym kliknięciem, bez hasła. RemoteConn robi to jednak tak, że **serwer nie może wpuścić nikogo
do Twoich komputerów**, nawet gdyby ktoś go przejął.

**Jak to działa:**
1. Z hasła konta aplikacja liczy na Twoim komputerze klucz konta (Argon2id, 64 MiB). Serwer dostaje tylko klucz
   publiczny i sól, nigdy hasło.
2. Logowanie: aplikacja podpisuje kluczem konta jednorazowe wyzwanie od serwera.
3. Przy logowaniu urządzenie wystawia sobie **certyfikat**: kluczem konta podpisuje swój klucz urządzenia (ważny rok,
   każde logowanie go odnawia). Klucz prywatny konta znika z pamięci zaraz potem.
4. Komputer na koncie zapamiętuje klucz publiczny konta (opcja „Wpuszczaj moje urządzenia bez hasła”). Gdy łączy się
   urządzenie z ważnym certyfikatem tego konta, host wpuszcza je bez hasła i bez okna akceptacji. Szyfrowanie Noise
   dowodzi przy tym, że urządzenie ma klucz z certyfikatu, więc skopiowany certyfikat na nic się nie przyda.

**Konta zakłada się z kodem zaproszenia** (domyślnie, `RC_ACCOUNTS=invite`): administrator tworzy kod w panelu
(zakładka Konta) albo `rc-admin account invite`, a użytkownik wpisuje go w Ustawienia → Konto → Nowe konto.
Można też pozwolić każdemu (`open`) albo wyłączyć konta (`off`).

**Utrata urządzenia:** Ustawienia → Konto → „Odbierz dostęp” przy urządzeniu (albo administrator w panelu). Serwer
od razu powiadamia komputery konta, które są online, a te przestają wpuszczać to urządzenie. Hosty tylko dopisują
odebrane urządzenia do listy, nigdy ich z niej nie usuwają, więc przejęty serwer nie przywróci dostępu skradzionemu
laptopowi.

**Zmiana hasła:** stary klucz podpisuje nowy. Komputery konta przyjmują nowy klucz automatycznie (sprawdzają podpis,
więc serwer nie może podsunąć własnego klucza), a certyfikaty starego klucza przestają działać: na pozostałych
urządzeniach, z których się łączysz, zaloguj się ponownie.

| Kto przejmie… | może | nie może |
|---|---|---|
| serwer | blokować połączenia, widzieć listę urządzeń kont | wpuścić kogokolwiek do Twoich komputerów, poznać hasła, podmienić klucza konta |
| hasło do konta | łączyć się z Twoimi komputerami (jak właściciel) | nic więcej; po zmianie hasła i odebraniu dostępu urządzeniom traci dostęp |
| kopię pliku z certyfikatem | nic, bez klucza prywatnego urządzenia (DPAPI) | — |

**Administrator** w panelu (zakładka Konta) widzi konta i ich urządzenia, może tworzyć zaproszenia, blokować
i usuwać konta oraz odbierać dostęp urządzeniom. Zablokowanie konta działa od razu na komputerach online. Resetu
hasła nie ma, bo serwer go nie zna: konto trzeba usunąć i założyć od nowa, a komputery trzeba zalogować ponownie
(to celowe: administrator serwera nie może przejąć cudzych komputerów).

## 8. Rozwiązywanie problemów

| Objaw | Co sprawdzić |
|---|---|
| „Serwer ID jest nieosiągalny” | adres serwera w Ustawienia → Sieć („Testuj”); na serwerze otwarty port UDP 21116 |
| „Serwer ID odrzucił klucz dostępu” | klucz w Ustawienia → Sieć; administrator sprawdzi go poleceniem `rc-admin key show` |
| „Klucz serwera ID jest inny niż przy pierwszym połączeniu” | czy serwer był reinstalowany; jeśli nie, możliwy atak; jeśli tak: „Zapomnij klucz serwera” |
| ID nie pojawia się po włączeniu udostępniania | adres serwera i klucz; najedź na status „offline”, żeby zobaczyć przyczynę |
| Zawsze „Relay” zamiast „P2P” | normalne przy NAT symetrycznym, CGNAT i w sieciach komórkowych; działa przez serwer |
| „Ten komputer nie wpuszcza Twojego konta bez hasła” | na komputerze zdalnym zaloguj się na to samo konto i włącz „Wpuszczaj moje urządzenia bez hasła” |
| „Usługa nie dostała nowych ustawień” | zgoda w oknie UAC została odrzucona; Ustawienia → Usługa → „Przekaż ustawienia do usługi” |
| Usługa zainstalowana, ale okno pokazuje „Łączenie z usługą…” | agent startuje kilka sekund; dziennik: `C:\ProgramData\RemoteConn\service\agent.log` (jako administrator) |
| „Połącz” w panelu serwera nic nie robi | aplikacja musi być choć raz uruchomiona na tym komputerze (rejestruje wtedy linki `remoteconn://`) |

## 9. Przydatne ścieżki

| Ścieżka | Zawartość |
|---|---|
| `%APPDATA%\com.remoteconn.desktop\` | dane aplikacji: ustawienia (`config.json`), klucz urządzenia, stałe hasło i sesja konta (DPAPI) |
| `C:\Program Files\RemoteConn\remoteconn-service.exe` | program usługi (kopia aplikacji, zapis tylko dla administratorów) |
| `C:\ProgramData\RemoteConn\service\` | dane usługi: ustawienia, klucz urządzenia, stałe hasło (DPAPI konta SYSTEM), dzienniki `service.log` i `agent.log`; dostęp tylko SYSTEM i Administratorzy |
| `Pobrane\RemoteConn` | odebrane pliki (folder można zmienić w Ustawienia → Sesja) |
