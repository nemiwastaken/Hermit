# Hermit

Домашний агент: живёт на твоём сервере, управляется из Telegram, работает с
файлами и командами в изолированном контейнере. Профиль, память и скиллы лежат в
git — поведение меняется коммитом и откатывается одной командой.

## Как устроено

Главное правило: **ядро ничего не исполняет**. Оно умеет только «имя инструмента
+ JSON аргументов → JSON результат» и говорит с контейнерами по unix-сокету.
Модель не может выполнить код в ядре — у неё нет такого пути. У `bash` нет сети
никогда, а `web_fetch` не видит ни файлов, ни базы.

| Часть | Что делает | Где живёт |
|---|---|---|
| **Ядро** `core/` | LLM, очередь, Telegram, политика инструментов | бинарник на хосте, systemd |
| **Поведение** `behavior/` | профиль, память, скиллы — попадают в промпт | git-репо `/opt/hermit/behavior` |
| **Песочница** `sandbox/` | исполняет `bash`, файлы, поиск, процессы | Docker без сети |
| **Сеть** `net/` | `web_fetch`, `web_search`, MCP | Docker, единственный выход в интернет |
| **Рабочая папка** `workspace/` | всё, с чем работает агент | `/opt/hermit/workspace` |

В репозитории только `core/`, `sandbox/`, `net/` и установщик. `behavior/` и
`workspace/` создаются при установке; в чекауте для разработки их заменяет
игнорируемый `.dev/`.

## Возможности

- **Telegram**: long-poll, только личные чаты и только из белого списка. Ответ
  стримится в одно сообщение, «печатает…» висит и во время вызова инструментов.
- **Очередь в SQLite**: сообщение переживает перезапуск и падение.
- **Модель**: любой OpenAI-совместимый endpoint, по умолчанию DeepSeek.
- **Инструменты**: `bash` (argv, без шелла), файлы `read`/`write`/`edit`/`patch`,
  поиск `glob`/`grep`/`list`, фоновые процессы `process_manage` и `execute_code`
  — Python, который сам вызывает инструменты.
- **Интернет**: `web_search` (DuckDuckGo без ключа) и `web_fetch` (текст страницы
  в markdown) из отдельного контейнера без `/workspace`. Браузера нет: JS и
  логины не читаются.
- **Память и поведение**: `USER.md` и `MEMORY.md` в git, изменения через
  инструменты с коммитом, `/rollback` откатывает последнее.
- **Скиллы**: в промпте только имя и одна строка описания, тело читает
  `skill(name)` — меньше токенов на каждый ход.
- **Сессии**: закрытые диалоги уходят в архив; прошлые находятся вопросом,
  `/sessions` или `recall`. Старше 30 дней — удаляются.
- **Контекст**: `/compress` заменяет переписку сводкой и последним обменом; при
  100k символов это происходит само.
- **Расписания без модели**: напоминания и трекеры — вопрос, число и среднее
  считает SQLite, а не LLM.
- **Расход**: `/usage` и `/insights` показывают токены и цену из SQLite.
- **MCP**: удалённые HTTP-серверы добавляют свои инструменты на старте.
- **Защита**: опасные команды (`rm -rf`, `mkfs`, `dd`, `reboot`…) ждут кнопки в
  Telegram; `/dlq` и `/retry` разбирают упавшие события.

## Требования

Linux, systemd, Docker с compose. Rust нужен только для сборки — ставь через
rustup от своего пользователя, установщик сам найдёт `cargo`.

## Установка

```sh
curl -fsSL https://raw.githubusercontent.com/nemiwastaken/Hermit/main/install.sh | sudo bash
```

Установщик скачает исходники, соберёт ядро и образы, создаст каталоги, заведёт
git-репозиторий поведения, поставит systemd-юнит и запустит всё. Спросит токен
бота и ключ LLM. Версия и зеркало — через `HERMIT_REF` и `HERMIT_SRC`, флаги
`--no-start`, `--skip-build`, `--no-systemd` передаются после `--`.

Из клона:

```sh
git clone https://github.com/nemiwastaken/Hermit ~/hermit && cd ~/hermit
sudo ./install.sh
```

Повторный запуск — это обновление: конфиг, база и `behavior/` не перезаписываются,
бинарник подменяется атомарно. Всё лежит в `/opt/hermit` (`hermit.env` с
секретами, `behavior/`, `workspace/`, `data/hermit.db`, `mcp.json`), кроме юнита
systemd. Другой префикс: `sudo PREFIX=/srv/agent ./install.sh`.

### Первый запуск

1. Напиши боту в личку. Если тебя нет в списке, он ответит твоим user id.
2. Впиши id в `TELEGRAM_ALLOWED_USER_IDS` в `/opt/hermit/hermit.env`.
3. `sudo systemctl restart hermit`.
4. Проверка: «что лежит в /workspace?». Логи: `journalctl -u hermit -f`.

## Настройка

Всё в `/opt/hermit/hermit.env` (формат `KEY=value`, режим 600):

| Переменная | По умолчанию | Смысл |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | — | токен от @BotFather, обязателен |
| `TELEGRAM_ALLOWED_USER_IDS` | пусто | кому отвечать: user id через запятую |
| `TELEGRAM_ALLOWED_CHAT_IDS` | пусто | старое имя, для личек тот же id |
| `LLM_BASE_URL` | `https://api.deepseek.com/v1` | OpenAI-совместимый endpoint |
| `LLM_API_KEY` | — | ключ провайдера, обязателен |
| `LLM_MODEL` | `deepseek-chat` | модель |
| `LLM_MAX_TOKENS` | не задано | ограничение ответа |
| `LLM_PRICE_IN_PER_M` | `0` | цена входных токенов, USD за 1M |
| `LLM_PRICE_OUT_PER_M` | `0` | цена выходных токенов, USD за 1M |
| `DATABASE_PATH` | `/opt/hermit/data/hermit.db` | SQLite |
| `SANDBOX_SOCKET` | `/opt/hermit/run/sandbox.sock` | сокет песочницы |
| `BEHAVIOR_DIR` | `/opt/hermit/behavior` | профиль, память, скиллы |

Без цен `/usage` показывает только токены.

## Команды

Отвечаются сразу, модель не вызывается:

| Команда | Что делает |
|---|---|
| `/pause`, `/resume` | остановить и продолжить разбор очереди |
| `/safe`, `/safe off` | выключить и вернуть инструменты |
| `/status` | пауза, safe mode, очередь, контекст, архив |
| `/new`, `/clear` | закрыть диалог в архив / удалить контекст чата |
| `/sessions` | list, `<id>`, `resume`, `rename`, `delete` |
| `/memory` | профиль, память и скиллы, которые видит агент |
| `/compress` | сжать контекст в сводку |
| `/usage`, `/insights [дней]` | расход за сегодня и 7 дней / по дням |
| `/changes`, `/rollback` | история поведения и откат последнего изменения |
| `/dlq`, `/retry <id>` | упавшие события и возврат в очередь |
| `/help` | список |

## MCP

`/opt/hermit/mcp.json` — массив удалённых HTTP-серверов (Streamable HTTP):

```json
[
  {
    "name": "github",
    "url": "https://api.githubcopilot.com/mcp/",
    "headers": {"Authorization": "Bearer ..."},
    "approval": true
  }
]
```

Инструменты появляются у модели с именами `mcp__<сервер>__<инструмент>`.
`approval: true` требует кнопку на каждый вызов, `allow_private: true` разрешает
сервер в домашней сети (по умолчанию приватные адреса отбиваются как у
`web_fetch`). `stdio` не поддерживается. После правки:
`sudo systemctl restart hermit` и `docker compose -f /opt/hermit/compose.yml restart net`.

## Разработка

Всё, что появляется при запуске, живёт в игнорируемом `.dev/`:

```sh
cp .env.example .env && chmod 600 .env     # вписать токен и ключ
mkdir -p .dev/run .dev/workspace
cp -r behavior.example .dev/behavior
printf '[]\n' > .dev/mcp.json
docker compose up -d --build
cd core && cargo run --release
```

Тесты:

```sh
cd core && cargo test && cargo clippy --all-targets
python3 sandbox/server.py --selftest
docker run --rm --entrypoint python3 -v "$PWD/net:/app" hermit-net -m unittest -v
```

Протокол с песочницей — одна строка JSON на запрос и ответ:

```json
→ {"id":"e42/7","tool":"bash","args":{"cmd":["rg","-n","TODO","src"]}}
← {"id":"e42/7","ok":true,"exit":0,"stdout":"…","truncated":{"full_path":"/workspace/.tmp/e42_7-out.log"}}
```

`cmd` — argv, а не строка: ядро ничего не склеивает и не экранирует. Пайпы и
редиректы — только если модель сама попросит `["bash","-lc","…"]`, и это
исполняется внутри контейнера.

## Безопасность

- Песочница: `network_mode: none`, read-only rootfs, `cap_drop: ALL`,
  `no-new-privileges`, без секретов. `behavior/` смонтирован только на чтение.
- Бот отвечает только в личных чатах и только из белого списка; остальное
  отклоняется до создания события.
- Инструменты перечислены явно в `core/src/policy.rs`, всё остальное — отказ.
- Упавшее событие не ретраится само: повторный `bash` может повторить side
  effects. Оно паркуется как `dead` и видно в `/dlq`, причина — в журнале.
- Секреты лежат в `hermit.env` (600) на диске — это нижняя граница, а не
  хранилище секретов.

## Лицензия

См. `LICENSE`.
