# NexusLink

[English version / Английская версия → README.md](README.md)

Локальный API-шлюз, который позволяет **Claude Desktop** общаться с **любым
OpenAI-совместимым провайдером** (OpenRouter, Groq, Ollama, vLLM, LM Studio,
together.ai, …) — с ротацией провайдеров и ключей, встроенной панелью
управления и честными ошибками.

```
Claude Desktop ──Anthropic /v1/messages──▶ NexusLink (127.0.0.1:5679)
                                            │  auth + слоты + ротация + перевод
                                            ▼
                                     OpenAI /v1/chat/completions
                                            │
                                     API провайдера ──▶ настоящая LLM
                                            │
                OpenAI SSE-чанки ──▶ Anthropic SSE-события ──▶ Claude Desktop
```

Чистая реализация: архитектурные уроки взяты из разбора исходников ModelLink
(CC BY-NC-ND — **код не копировался**), семантика конвертации изучалась по
ccMesh (Apache-2.0); весь код здесь оригинальный, Apache-2.0.

## Что он умеет

| Задача | Поведение |
|---|---|
| **Входная авторизация** | Каждый запрос требует `Authorization: Bearer <incoming_token>` или `x-api-key`. Иначе → `401`. Loopback-биндинг авторизацией не является. |
| **Слоты моделей** | Claude Desktop знает только реальные ID Anthropic, поэтому каждая апстрим-модель получает *слот* (`claude-sonnet-5`, `claude-haiku-4-5`, …). Несмапленный слот → честный `400`, никаких скрытых подмен. Суффиксы дат (`claude-sonnet-5-20260101`) резолвятся в ту же модель. |
| **Ротация ключей** | У провайдера может быть несколько `api_keys`. На `401/402/403/429` ключ паркуется на 15 минут (маскированный тег `key|gateway|…tail`), а запрос уходит следующему ключу — запросы на заведомо мёртвые ключи не тратятся. |
| **Ротация провайдеров** | Несколько провайдеров могут маппить модели на **один слот** — они образуют цепочку фолбэков в порядке конфига. На ротируемых ошибках (`401/402/403/404/429/5xx/нет соединения`) NexusLink перебирает кандидатов. Мёртвые эндпоинты паркуются по модели целиком (`model|gateway|model-name`). |
| **Честное исчерпание** | Все кандидаты мертвы → `502` с прямым ответом — никакого поддельного успеха. Неротируемые ошибки апстрима (например `400`) проходят как есть. |
| **Перевод** | `system` → первое `role:system` сообщение · content-блоки → OpenAI messages · `tool_use` → `tool_calls` · `tool_result` → `role:"tool"` · `tools`+`tool_choice` → OpenAI function tools · `max_tokens` → `max_completion_tokens` (или `max_tokens` по флагу провайдера) |
| **Стриминг** | OpenAI delta-чанки → Anthropic SSE (`message_start` → `content_block_*` → `message_delta` → `message_stop`). Фрагменты аргументов tool-call приходят по `index` и переиздаются как `input_json_delta`. `reasoning_content` становится блоком `thinking`. Ротация происходит только **до** начала стрима — байты в полёте никогда не переключаются. |
| **Хартбит** | Если апстрим молчит **15 с**, в SSE вставляется комментарий `: ping`, чтобы `inferenceStreamIdleTimeoutSec` у Claude Desktop не убил соединение. |
| **Панель управления** | `nexuslink ui` поднимает Nexus Lux панель на `/` (EN/RU): провайдеры, пулы ключей, слоты, живой чёрный список ротации, apply в Claude Desktop. Токен приходит через URL-фрагмент — не попадает в query, логи и историю. |
| **Авто-компакт** | Если у модели указано окно `context`, запрос, не влезающий в него, подрезается sliding-window до отправки: системный промпт + якорное сообщение + хвост по бюджету, tool-группы атомарно. Апстрим-ошибка `context length` → ротация на следующего кандидата. |
| **Apply в Claude Desktop** | `nexuslink apply` пишет `Claude-3p/configLibrary/<uuid>.json` с `inferenceProvider: gateway`, URL+ключ шлюза и слотами `inferenceModels` — read-modify-write, атомарная запись. Также регистрирует MCP-сервер в `claude_desktop_config.json` и `~/.claude.json` и ставит скилл `nexuslink-video`. |

## Установка

Берите установщик из [Releases](../../releases/latest):

| Платформа | Файл | v0.2.0 |
|---|---|---|
| Windows | `.msi` установщик (ярлыки в «Пуске» и на рабочем столе) | [скачать](../../releases/download/v0.2.0/nexuslink-0.2.0-x86_64.msi) |
| Windows | `.zip` — один exe, ноль зависимостей | [скачать](../../releases/download/v0.2.0/nexuslink-0.2.0-windows-x86_64.zip) |
| macOS | `.dmg` universal — Apple Silicon + Intel | [скачать](../../releases/download/v0.2.0/nexuslink-0.2.0-macos-universal.dmg) |

Сборка для macOS без подписи разработчика — первый запуск: правый клик → **Открыть**.

**Из исходников**

```bash
cargo build --release   # один бинарь: сервер + десктоп + MCP + CLI
```

## Первый запуск

```bash
# 1) создать ~/.nexuslink/config.json с сгенерированным incoming_token
./nexuslink init

# 2) отредактировать конфиг — или открыть панель и сделать там
./nexuslink app             # десктоп-приложение: прокси в фоне + панель WebView2,
                            # X сворачивает в трей (правый клик → Выход)

./nexuslink ui              # поднимает прокси + открывает панель в браузере

# Claude Desktop спавнит это сам через mcpServers (пишется `apply`):
./nexuslink mcp             # stdio MCP: generate_video / video_status (Agnes)

# headless: только прокси
./nexuslink serve           # слушает 127.0.0.1:5679

# 3) подключить Claude Desktop (пишет его configLibrary)
./nexuslink apply           # затем полностью перезапустите Claude Desktop
```

Ещё подкоманды: `nexuslink slots` — текущие маппинги слот → модель;
`nexuslink status` — апстрим-статус.

**Миграция с openbridge:** если есть `~/.openbridge/config.json`, а
`~/.nexuslink/config.json` нет — старый файл читается автоматически; следующее
сохранение ляжет в новое место. Легаси-файл не изменяется.

## Конфигурация (`~/.nexuslink/config.json`)

```jsonc
{
  "incoming_token": "nl-…",            // генерируется `init`; это же ключ шлюза
                                        // в Claude Desktop
  "port": 5679,
  "providers": [
    {
      "name": "OpenRouter",
      "base_url": "https://openrouter.ai/api",   // без /v1 в конце
      "api_keys": ["sk-or-…A", "sk-or-…B"],       // пул: ротация на 401/402/403/429
      // "api_key": "sk-or-…"                    // одноключевой шорткат тоже работает
      // опциональные ручки:
      "chat_path": "",                            // дефолт "/v1/chat/completions"
      "supports_reasoning_effort": false,         // многие провайдеры 400-ят его
      "use_max_completion_tokens": true,          // false -> шлём max_tokens
      "models": [
        { "name": "meta-llama/llama-3.1-70b-instruct", "slot": "claude-sonnet-5" },
        { "name": "qwen/qwen-2.5-72b-instruct",        "slot": "claude-haiku-4-5" }
      ]
    },
    {
      // второй провайдер на ТОМ ЖЕ слоте -> фолбэк для claude-sonnet-5
      "name": "Groq",
      "base_url": "https://api.groq.com/openai",
      "api_key": "gsk_…",
      "models": [
        { "name": "llama-3.3-70b-versatile", "slot": "claude-sonnet-5" }
      ]
    },
    {
      "name": "Ollama (local)",
      "base_url": "http://localhost:11434",
      "chat_path": "/v1/chat/completions",
      "api_key": "ollama",                        // любая непустая строка
      "models": [
        { "name": "llama3.1:70b", "slot": "claude-opus-4-6" }
      ]
    }
  ]
}
```

**Семантика ротации** (как у торгового LLM-пула):

- Кандидаты слота — все тройки `(provider, model, key)`, маппящиеся на него,
  в порядке провайдеров; ключи внутри провайдера крутятся round-robin.
- `401/402/403/429` → паркуется **ключ** на **этом шлюзе** на 15 минут.
- `404`/`5xx`/нет соединения → паркуется **модель** на **этом шлюзе**; после
  повторных фейлов модель пропускается без траты запросов.
- Припаркованные ключи пропускаются до отправки; в панели видно маску
  (`…tail6`) и остаток TTL.
- Чёрный список живёт в памяти — рестарт его чистит (зациклившийся провайдер
  получает новый шанс — это желаемое поведение).

**Пул слотов** (ID, которые знает Claude Desktop — используйте их):

```
claude-opus-5  claude-sonnet-5  claude-opus-4-8  claude-opus-4-7
claude-opus-4-6  claude-sonnet-4-6  claude-sonnet-4-5  claude-haiku-4-5
```

Новые модели получают слоты в порядке предпочтения (`claude-sonnet-5` первым —
туда ходят safety-проверки Auto-режима Claude). Назначение слотов **хранится**,
не позиционное — добавление/удаление моделей не перетасовывает маппинги.

## Панель управления

`nexuslink ui` открывает `http://127.0.0.1:<port>/#t=<token>` — токен едет во
фрагменте и не попадает в логи, историю и на провод. Без фрагмента показывается
гейт токена; `/api/status` открыт (отдаёт только маскированные хвосты, счётчики
и аптайм), а `/api/config` и `/api/apply` требуют токен.

Панель (English/Русский) редактирует тот же `config.json` — PUT пишет атомарно
и горячо перезагружает работающий прокси, без рестарта.

## Настройка Claude Desktop

1. `nexuslink apply` пишет файл configLibrary и печатает путь.
   Ставит: `inferenceProvider: "gateway"`,
   `inferenceGatewayBaseUrl: "http://127.0.0.1:<port>"`,
   `inferenceGatewayApiKey: "<incoming_token>"`, `deploymentMode: "3p"`,
   `inferenceModels: [ваши слоты]`. Также регистрирует MCP-сервер `nexuslink`
   (`generate_video` / `video_status`) в `claude_desktop_config.json` **и**
   `~/.claude.json` (юзерскоуп Claude Code — именно оттуда читают встроенные
   сессии), и ставит скилл `nexuslink-video` в `~/.claude/skills/`.
2. Полностью перезапустите Claude Desktop (трей → выход, не просто закрыть окно).
3. Выберите модель — селектор показывает реальное имя через `labelOverride`.

Ручной путь (без `apply`): в настройках third-party/gateway у Claude Desktop
задайте base URL `http://127.0.0.1:5679`, API key = ваш `incoming_token`,
добавьте имена слотов как модели.

## Смоук-тест

```bash
./scripts/smoke.sh                 # auth, models, stream, unmapped-400, status
```

С фейковым апстримом (любой OpenAI-мок) направьте `base_url` на него и поставьте
первым в `api_keys` мёртвый ключ — смоук покажет ротацию на живой ключ и
припаркованную запись в `/api/status`.

## Заметки по безопасности

- **Биндинг только loopback** — закрывает удалённый доступ, но **не** локальные
  процессы; bearer-токен — то, что реально аутентифицирует.
- Ключи хранятся открытым текстом в `~/.nexuslink/config.json` — та же модель
  угроз, что у ModelLink; на Unix — chmod 600. Не коммитьте его.
- API-ключ провайдера шлётся как `Authorization`; входящий bearer-токен
  **никогда** не форвардится апстрим.
- URL/ключи провайдеров не логируются; в чёрном списке — только последние
  6 символов ключа. Тела запросов не логируются.

## Структура

```
src/config.rs      файл конфига, пул слотов, резолв кандидатов, атомарное сохранение
src/rotation.rs    TTL-чёрный список, классификация ошибок, порядок кандидатов
src/translator.rs  типизированные Anthropic<->OpenAI структуры + маппинги
src/streaming.rs   stateful конвертер OpenAI-чанков -> Anthropic-SSE
src/proxy.rs       axum-роутер, bearer-auth, цикл ротации, SSE-мост,
                   sliding-window авто-компакт по лимитам контекста
src/ui.rs          API управления (/api/status|config|apply) + вшитый SPA
src/gateway.rs     configLibrary Claude-3p + mcpServers + установщик скилла
src/desktop.rs     frameless WebView2-оболочка, инжектный тайтлбар, трей,
                   скругления DWM, запоминание геометрии окна
src/mcp.rs         stdio MCP-сервер — generate_video / video_status (Agnes)
src/metrics.rs     per-скоуп статистика (попытки, EMA-латентность)
src/main.rs        CLI: serve / app / ui / init / apply / slots / status / mcp
ui/index.html      Nexus Lux панель — EN/RU, провайдеры, пулы, чёрный список,
                   метрики; в debug перечитывается с диска на каждый запрос
skills/            скилл Claude Code, ставится `apply` (nexuslink-video)
wix/main.wxs       определение MSI (cargo wix)
packaging/macos/   Info.plist + bundle.sh (.app + .dmg системными средствами)
tests/             контрактные тесты перевода + интеграционные ротации
scripts/smoke.sh   end-to-end curl-проверки
```

## Релизы

CI собирает всё по тэгам `v*`: `cargo wix` MSI + zip с exe на
`windows-latest`, lipo-универсальный бинарь → `NexusLink.app` → `.dmg` на
`macos-latest`, затем публикует GitHub-релиз. Локальные проверки перед
тэгированием: `cargo check`, `cargo test`, `cargo build --release`.

## Известные ограничения

- Блоки `thinking` / `output_config.effort` принимаются и отбрасываются, если
  провайдер не включил `reasoning_effort` — эквивалента в OpenAI нет; молча
  переписывать бюджет мышления было бы ложью.
- `image`-блоки и `cache_control` пока не переводятся — парсятся, но апстрим
  не уходят.
- Как только стрим начал отдавать байты, апстрим-фейл в середине выглядит как
  обрезанный SSE — ротация никогда не подменяет провайдера посреди ответа.
