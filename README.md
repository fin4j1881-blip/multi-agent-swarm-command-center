# Swarm Command Center

Tauri v2 + Rust tabanlı, hibrit kaynaklı çoklu ajan masaüstü uygulaması.
Bir **Komutan (Architect)** modeli projeyi planlar, bir **İşçi (Coder)** modeli kodu üretir,
Komutan kodu denetler ve yalnızca onaylanan dosyalar diske yazılır.

Her iki rol için kaynak bağımsız seçilir:

| Kaynak | Ne yapar | Anahtar gerekir mi? |
| --- | --- | --- |
| **API** | OpenRouter / Anthropic / OpenAI / DeepSeek / OpenAI-uyumlu özel endpoint'e doğrudan HTTP | Evet (veya ortam değişkeni) |
| **IDE / Yerel Oturum** | Kurulu IDE/CLI oturumunu (`opencode run`, `claude -p`, `cursor-agent -p`) veya localhost HTTP köprüsünü kullanır | Hayır |

---

## 1. Mimari

```
┌──────────────────────────────────────────────────────────────┐
│  UI (ui/index.html + main.js, Tauri WebView)                 │
│  invoke: start_command_center / stop / select_folder / ...   │
└───────────────┬──────────────────────────────────────────────┘
                │ Tauri IPC
┌───────────────▼──────────────────────────────────────────────┐
│  Rust backend (tokio, Arc<Mutex<AppState>>)                  │
│                                                              │
│  ModelProvider trait                                         │
│   ├── ApiProvider        → reqwest (OpenAI / Anthropic)      │
│   └── IdeBridgeProvider  → tokio::process::Command | HTTP    │
│                                                              │
│  Orchestrator                                                │
│   1. proje ağacını tara                                      │
│   2. Komutan → JSON görev planı                              │
│   3. İşçi → tam dosya içeriği                                │
│   4. yerel guardrail (bedava)                                │
│   5. Komutan denetimi (onay / ret + revizyon)                │
│   6. onay → yedek al → tokio::fs ile yaz                     │
└──────────────────────────────────────────────────────────────┘
```

Denetim döngüsü (görev başına en fazla `maxAttempts`, varsayılan 3):

```
İşçi kodu üretir
   ↓
Yerel guardrail: boş/kesik içerik, "..." yer tutucu, markdown sarmalı,
                 path traversal, aynı içerik, bozuk JSON, parantez dengesi
   ↓ (sorun yoksa)
Komutan denetimi: {"approved": bool, "score": 0-10, "issues": [...], "revision": "..."}
   ↓ onay
.swarm/backups/<zaman>/<dosya> yedeği → atomik tam dosya yazımı
   ↓ (opsiyonel)
cargo check → hata varsa dosya yedeğe geri alınır, revizyon istenir
```

Kod asla doğrudan diske yazılmaz; ret edilen öneri sadece log'da görünür.

---

## 2. Klasör yapısı

```
.
├── src-tauri/
│   ├── Cargo.toml
│   ├── build.rs
│   ├── tauri.conf.json
│   ├── capabilities/default.json
│   ├── icons/
│   └── src/
│       ├── main.rs           # Tauri başlatıcı
│       ├── lib.rs            # IPC komutları
│       ├── config.rs         # AgentConfig / ProviderConfig
│       ├── provider.rs       # ModelProvider trait + factory
│       ├── api_provider.rs   # OpenAI-uyumlu + Anthropic Messages
│       ├── ide_bridge.rs     # CLI (argv/stdin) + localhost HTTP köprüsü
│       ├── orchestrator.rs   # plan → kod → denetim → yaz döngüsü
│       ├── guardrail.rs      # yerel, maliyetsiz ön denetim
│       ├── prompts.rs        # Komutan / İşçi / denetçi prompt sözleşmeleri
│       ├── protocol.rs       # JSON & markdown protokol parser'ları
│       ├── workspace.rs      # güvenli path, ağaç tarama, yedek, yazma
│       └── state.rs          # AppState, log/event yayını
├── ui/
│   ├── index.html
│   ├── style.css
│   └── main.js               # framework yok, window.__TAURI__ ile IPC
├── examples/
│   └── ide_bridge.py         # localhost HTTP köprüsü örneği (stdlib)
└── README.md
```

Frontend bilerek derlemesiz: `frontendDist = "../ui"` olduğu için `npm`, Vite veya
bir dev server gerekmez.

---

## 3. Gereksinimler

- **Rust** 1.77+ (bu depo 1.99 ile derlendi)
- **Tauri CLI v2**
  ```bash
  cargo install tauri-cli --version "^2" --locked
  # veya: npm install -D @tauri-apps/cli
  ```
- **Linux sistem paketleri** (Debian/Ubuntu)
  ```bash
  sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
    libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
  ```
  Arch: `webkit2gtk-4.1 base-devel libxdo openssl librsvg`
- **macOS**: Xcode Command Line Tools
- **Windows**: MSVC Build Tools + WebView2 (Windows 10/11'de hazır gelir)

Node.js gerekmez.

---

## 4. Çalıştırma

```bash
cd "Multi-Agent Swarm Command Center"

# geliştirme (UI derleme adımı olmadan paketlenir, npm/Vite yok)
cargo tauri dev

# üretim derlemesi (Linux'ta yalnızca .deb için: --bundles deb)
cargo tauri build
```

İlk derleme Tauri bağımlılıkları için birkaç dakika sürebilir. Sonraki açılışlar hızlıdır.
Uygulama boştayken ağ isteği, zamanlayıcı veya CPU döngüsü çalıştırmaz; yalnızca
"Komutanlığı Başlat" sonrası tokio görevi uyanır.

### Testler

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib
# 30 test: plan/proposal/verdict parser'ları, path güvenliği, guardrail, model listesi, fallback zinciri
```

---

## 5. Hibrit model kurulumu

### 5.1 Seçenek A — API

UI'da `Kaynak: API` seç, sağlayıcıyı belirle; **model adını elle yazmıyorsun**.

| Sağlayıcı | Varsayılan endpoint | Anahtar ortam değişkeni | Model örneği |
| --- | --- | --- | --- |
| OpenRouter | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | `deepseek/deepseek-chat`, `anthropic/claude-sonnet-4` |
| Anthropic | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` | `claude-sonnet-4-20250514` |
| OpenAI | `https://api.openai.com/v1` | `OPENAI_API_KEY` | `gpt-4.1-mini`, `o4-mini` |
| DeepSeek | `https://api.deepseek.com/v1` | `DEEPSEEK_API_KEY` | `deepseek-chat`, `deepseek-reasoner` |
| Custom | kullanıcı girer | `LLM_API_KEY` | Ollama/LM Studio/vLLM modelleri |

- **Endpoint otomatik gelir**: sağlayıcı değişince Base URL alanı o sağlayıcının
  varsayılanıyla dolar. Elle değiştirirsen korunur; `sıfırla` ile varsayılana dönersin.
  Custom seçeneğinde alan boş kalır, adresi sen girersin.
- **Model listesi otomatik gelir**: anahtar girildiğinde (veya `⟳` düğmesine
  basıldığında) backend sağlayıcının `/models` endpoint'ini çağırır ve model
  `<select>` kutusunu doldurur. Model adı serbest metin değildir.
  - OpenRouter/OpenAI/DeepSeek: `GET {base}/v1/models`
  - Anthropic: `GET https://api.anthropic.com/v1/models` (`anthropic-version` başlığı)
  - Custom: `GET {base}/v1/models` (endpoint `/chat/completions` ile bitiyorsa kırpılır)
- Liste alınamazsa alanın altında kırmızı bir not görünür (anahtar yok, 401, ağ hatası).
  Bu durumda son çare olarak listedeki **`Özel model adı…`** seçeneğiyle adı elle
  girebilirsin; `/models` sunmayan gateway'ler için vardır.
- Endpoint çağrı sırasında otomatik tamamlanır: `.../v1` ile bitiyorsa
  `/chat/completions`, Anthropic'te `/v1/messages` eklenir. Tam endpoint girersen dokunulmaz.
- Anahtar alanı boşsa ortam değişkeni okunur. Böylece anahtarı UI'ya hiç yazmadan
  çalıştırabilirsin:
  ```bash
  export OPENROUTER_API_KEY="sk-or-..."
  cargo tauri dev
  ```
- 429 / 5xx yanıtlarında 1-2-4 saniyelik backoff ile 3 kez denenir.
- **Fallback (Yedek) Model desteği**: Seçilen sağlayıcının model listesinden
  ikinci bir yedek model seçilebilir. Birincil model kota/limit hatası verirse
  (HTTP 429, `insufficient_quota`, bakiye, context uzunluğu, 503 aşırı yük vb.),
  sistem otomatik olarak yedek modele geçer ve oturum boyunca oradan devam eder.
- UI anahtarları `localStorage`'a **yazmaz**; pencere kapanınca unutulur. Model
  seçimleri de oturumluktur; her açılışta liste yeniden çekilir.

> İpucu: Komutan için güçlü bir model (Claude Sonnet, GPT-4.1, DeepSeek Chat),
> İşçi için daha ucuz/hızlı bir model seçmek maliyeti düşürür.

### 5.2 Seçenek B — IDE / yerel oturum

`Kaynak: IDE / Yerel Oturum` seçildiğinde backend, seçilen IDE'nin **kendi oturumunu**
kullanır; ayrıca API anahtarı gerekmez. Komutlar proje kökünde (`cwd`) çalıştırılır,
böylece IDE aracı projeyi görür.

#### OpenCode CLI

```bash
# kurulum + oturum
opencode auth login
# test
opencode run --model openrouter/deepseek/deepseek-chat "sadece OK yaz"
```

UI: `IDE = OpenCode CLI`. Model açılır listeden seçilir (`opencode models` çıktısından
otomatik dolar, `⟳` ile yenilenir). Boş bırakırsan CLI'nin varsayılanı kullanılır.
Çalıştırılan komut: `opencode run [--model <model>] "<prompt>"`.

Uygulama `opencode` komutunu PATH'te arar; bulamazsa bilinen kurulum yerlerini dener
(`/usr/lib/opencode-desktop/opencode-cli`, `~/.opencode/bin/opencode`,
`~/.local/bin/opencode`, `/usr/local/bin/opencode`, `/snap/bin/opencode`).
Farklı bir konumdaysa `OPENCODE_BIN=/tam/yol/opencode` ile başlat.

#### Claude Code CLI

```bash
claude          # ilk giriş / oturum
claude -p "sadece OK yaz"
```

UI: `IDE = Claude Code CLI`. Model listesi `sonnet`, `opus`, `haiku` alias'larından
gelir (Claude Code model listesi komutu sunmaz); boş bırakılırsa CLI varsayılanı.
Prompt **stdin** üzerinden gönderilir
(`claude -p --model <model>`), böylece uzun promptlar komut satırı sınırına takılmaz.

#### Cursor Agent CLI

```bash
cursor-agent login
cursor-agent -p "sadece OK yaz"
```

UI: `IDE = Cursor Agent CLI`. Model listesi `cursor-agent models` çıktısından gelir;
alınamazsa boş kalır ve CLI varsayılanı kullanılır. Prompt argüman olarak eklenir.

#### Custom CLI

Herhangi bir CLI'ı bağlayabilirsin:

- **Komut**: `aider` (veya tam yol: `/usr/local/bin/aider`)
- **Argümanlar**: `--message {prompt} --yes`

`{prompt}` yer tutucusu prompt ile değiştirilir. Yer tutucu yoksa prompt sona eklenir.
Programın PATH'te olması gerekir; macOS'ta GUI uygulamaları shell PATH'ini miras almaz —
bu durumda komut alanına tam yolu yaz veya uygulamayı terminalden başlat.

#### Localhost HTTP köprüsü

`IDE = Custom HTTP köprüsü` + `Endpoint` (örn. `http://127.0.0.1:4319`).
Backend şu sözleşmeyi konuşur:

```http
GET /models HTTP/1.1
-> { "data": [ { "id": "qwen2.5-coder:7b", "name": "qwen2.5-coder:7b" } ] }

POST / HTTP/1.1
Content-Type: application/json

{ "model": "...", "system": "...", "prompt": "...", "stream": false }
```

Kabul edilen yanıt şekilleri (herhangi biri):

```json
{ "content": "..." }
{ "text": "..." }
{ "output": "..." }
{ "message": { "content": "..." } }
{ "choices": [ { "message": { "content": "..." } } ] }
{ "parts": [ { "text": "..." } ] }
```

Hazır örnek (stdlib Python, yerel Ollama'ya forward eder):

```bash
# terminal 1
ollama serve
# terminal 2
python3 examples/ide_bridge.py
# UI: Endpoint = http://127.0.0.1:4319
```

`examples/ide_bridge.py` içindeki `UPSTREAM` ve `MODEL` değişkenlerini kendi
IDE/yerel model sunucuna göre değiştirebilirsin. IDE'nin kendi HTTP API'si varsa
tek yapman gereken, gelen `prompt`/`system` alanlarını o API'ye taşımak.

> Not: Unix domain socket / CDP tabanlı özel köprüler bu sürümde yok. HTTP veya CLI
> kullan; soket yolu verirsen backend açık bir hata döndürür.

---

## 6. Guardrail ve dosya güvenliği

Yerel ön denetim (API çağrısı harcamadan reddeder):

- boş / 20 karakterden kısa içerik
- `... existing code`, `// ...`, `TODO: implement`, `aynı kalır` gibi yer tutucular
- markdown kod bloğu ile sarılmış veya `### FILE:` başlığı taşıyan içerik
- proje kökü dışına çıkan yollar (`..`, mutlak yol, `~`, `.git/`, `.swarm/`)
- mevcut dosyayla birebir aynı içerik (no-op)
- bozuk JSON dosyası; şüpheli parantez dengesi (uyarı olarak)
- ikili uzantıya (`.png`, `.zip`, …) metin yazma denemesi

Komutan denetimi ayrıca şunları reddeder: eksik import/tanımsız sembol, kesilmiş kod,
kapsam dışı refactor, vizyondan sapma, kabul kriterlerinin karşılanmaması.

Yazma güvenliği:

- Onaydan önce eski dosya `.swarm/backups/<YYYYMMDD-HHMMSS-mmm>/<göreli yol>` altına kopyalanır.
- Dosya, `tokio::fs::write` ile tam içerik olarak yazılır (diff değil).
- `Dry-run` açıksa hiçbir şey yazılmaz; sadece raporlanır.
- `cargo check` doğrulaması açıksa hata durumunda dosya yedeğe geri alınır ve revizyon istenir.
- "Durdur" iptal bayrağıdır: devam eden model çağrısı biter, sonra döngü durur; yarım dosya kalmaz.

Ayrıca `.gitignore` içinde `.swarm/` ve `.env` hazırdır.

---

## 7. IPC komutları

| Komut | Parametre | Dönüş |
| --- | --- | --- |
| `select_folder` | — | `string \| null` |
| `start_command_center` | `{ config: AgentConfig }` | proje kök yolu |
| `stop_command_center` | — | `void` |
| `get_status` | — | `{ running, stats, logs }` |
| `check_provider` | `{ config: ProviderConfig, projectPath?: string }` | test yanıtı |
| `list_models` | `{ config: ProviderConfig, projectPath?: string }` | `{ models: [{ id, name, context? }], source, note }` |

Canlı olaylar: `agent-log` (her adım) ve `agent-status` (`{ running }`).
`AppState` `Arc<tokio::sync::Mutex<...>>` içinde tutulur, log'lar 800 kayıtla sınırlıdır.

---

## 8. Sorun giderme

| Belirti | Çözüm |
| --- | --- |
| `webkit2gtk-4.1` bulunamadı | Bölüm 3'teki sistem paketlerini kur |
| `'opencode' başlatılamadı` | CLI kurulu değil veya PATH'te değil; tam yol gir |
| `401 / 403` | API anahtarı yanlış ya da model erişimi yok |
| `Model listesi için ... API anahtarı gerekli` | Model kutusu anahtar olmadan dolmaz; anahtarı gir veya ortam değişkenini tanımla |
| `Model listesi alınamadı` | `/models` endpoint'i kapalı olabilir; Base URL'i kontrol et ya da `Özel model adı…` ile devam et |
| `429` | Kota; backoff sonrası yine olursa modeli değiştir |
| `Komutan geçerli JSON üretmedi` | Komutan modeli zayıf; daha güçlü model seç, `Tur başına görev` değerini düşür |
| `İşçi kod bloğu üretmedi` | İşçi modele formatı hatırlatılır ve yeniden denenir; `maxAttempts` artırılabilir |
| `CLI ... saniye içinde yanıt vermedi` | CLI takıldı; süreç öldürülür. IDE oturumunu kontrol et |
| `IDE köprüsüne ulaşılamadı` | Köprü süreci çalışmıyor; `python3 examples/ide_bridge.py` veya kendi köprünü başlat |
| Pencere açılmıyor (Linux) | Wayland/X11 oturumunda çalıştır; `WEBKIT_DISABLE_COMPOSITING_MODE=1` dene |

---

## 9. Bilinçli sınırlar

- Model çağrıları **streaming değil**; tek yanıt alınır (log akışı ayrıdır).
- Ajan keyfi shell komutu çalıştırmaz; tek istisna opsiyonel `cargo check`.
- Yazma tam dosya bazlıdır; satır bazlı diff uygulanmaz.
- API anahtarları diske yazılmaz; kalıcı kullanım için ortam değişkeni önerilir.
- `maxAttempts` (varsayılan 3) aşılırsa görev atlanır ve sonraki göreve geçilir.

---

## 10. Lisans

Bu proje **Creative Commons Attribution-NonCommercial 4.0 International (CC BY-NC 4.0)** ile lisanslanmıştır.

- **Kullanım:** Kodları inceleyebilir, çalıştırabilir, değiştirebilir ve gayriticari amaçlarla kendi projelerinizde kullanabilirsiniz.
- **Kısıtlama:** Kodlar veya doğrudan türevleri **ticari amaçla kullanılamaz, satılamaz ve gelir getirici servislerin parçası yapılamaz**. Detaylar için `LICENSE` dosyasına bakınız.
