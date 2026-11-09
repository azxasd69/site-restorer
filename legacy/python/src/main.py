"""Site Restorer 2.1 — GUI (RU/EN) + CLI. Design pass: hierarchy, alignment, states."""
import argparse
import json
import os
import queue
import sys
import threading
import time
import traceback

from core import Cancelled, Engine
from sanitize import sanitize_site

APP_VERSION = "2.4.2"
CFG_DIR = os.path.join(os.environ.get("APPDATA", os.path.expanduser("~")), "SiteRestorer")
CFG_PATH = os.path.join(CFG_DIR, "config.json")

# ---- design tokens
BG = "#f3f4f6"
CARD = "#ffffff"
LINE = "#e5e7eb"
INK = "#111827"
MUTED = "#6b7280"
ACCENT = "#2563eb"
ACCENT_D = "#1d4ed8"
OK = "#059669"
WARN = "#b45309"
ERR = "#b91c1c"
FONT = "Segoe UI"

STR = {
    "ru": {
        "title": "Site Restorer — копия сайта из веб-архива",
        "h1": "Восстановление сайта из веб-архива",
        "sub": "Вставьте адрес — программа скачает копию сайта из Wayback Machine и соберёт её локально.",
        "site": "Адрес сайта",
        "site_ph": "например, prokupol.ru",
        "site_hint": "Можно вставить ссылку из письма, поиска или архива — адрес очистится автоматически.",
        "out": "Папка для копии",
        "browse": "Обзор…",
        "start": "Начать",
        "stop": "Остановить",
        "open": "Открыть папку",
        "help": "Справка",
        "lang": "Язык",
        "log": "Журнал",
        "log_empty": "Здесь появится подробный журнал: запросы, паузы архива, ошибки.",
        "stage": {
            "idle": "Готов к запуску",
            "inventory": "Инвентаризация архива",
            "download": "Скачивание файлов",
            "build": "Сборка оффлайн-копии",
            "verify": "Проверка ссылок",
            "report": "Формирование отчёта",
            "done": "Готово",
        },
        "step": "Этап %d из 4",
        "t_html": "HTML-страницы", "t_code": "CSS / JS / XML", "t_img": "Изображения", "t_other": "Прочие файлы",
        "idle_info": "Ожидание запуска",
        "wait": "Подготовка…",
        "stopping": "Остановка…",
        "speed": "скорость", "threads": "потоков", "left": "осталось", "ok": "успешно",
        "noarchive": "нет в архиве", "errors": "ошибок", "pause": "пауза",
        "done_msg": "Готово!\n\nКопия: %s\nОтчёт: %s",
        "err_title": "Ошибка",
        "need_site": "Введите адрес сайта, например: example.com",
        "need_out": "Укажите папку для копии.",
        "cancelled": "Остановлено. Повторный запуск продолжит с места остановки.",
        "help_title": "Справка — Site Restorer",
        "help_text": """ЧТО ДЕЛАЕТ ПРОГРАММА
Скачивает из Wayback Machine (веб-архив Internet Archive) все сохранённые страницы
и файлы указанного сайта и собирает статическую копию для локального просмотра.

ФОРМАТЫ АДРЕСА (можно вставлять как угодно)
prokupol.ru · https://www.prokupol.ru/catalog/ · PROKUPOL.RU
«prokupol.ru» · Сайт: prokupol.ru — ссылка из письма
ссылка из архива: web.archive.org/web/2026.../https://prokupol.ru/
ссылка из поиска: google.com/url?q=https://prokupol.ru/
адрес, набранный в русской раскладке: зкщлгзщдюкг
кириллический домен: прокупол.рф
Программа сама выделит домен, уберёт utm-метки, лишние пути и кавычки.

КАК ПОЛЬЗОВАТЬСЯ
1. Введите адрес сайта (например, prokupol.ru) — можно с https://, можно без.
2. Выберите папку для копии.
3. Нажмите «Начать». Кнопка «Остановить» прерывает работу; прогресс сохраняется,
   при повторном запуске уже скачанное не качается.

ИНДИКАТОРЫ
• Крупная полоса — общий прогресс по всем этапам.
• Надпись «Этап» — что происходит сейчас (4 этапа).
• Четыре полосы — HTML, CSS/JS/XML, изображения, прочие файлы (скачано / всего).
• Строка состояния — скорость, число потоков, остаток времени, объём, ошибки.
• Журнал — подробный лог, включая паузы при ограничениях архива.

ПОЧЕМУ ДОЛГО
Internet Archive ограничивает число запросов с одного IP. Программа:
• соблюдает паузы (Retry-After) и снижает число потоков при ответах 429;
• плавно повышает потоки, когда скорость растёт;
• автоматически делает паузу, если ограничения повторяются.
Это нормальная работа, а не сбой.

ЧТО ВОССТАНАВЛИВАЕТСЯ
Только то, что архив когда-либо сохранил. Файлы, которые краулер не забирал
(например, часть картинок товаров), отсутствуют — их нет ни в одной версии.
Динамические функции (корзина, вход, поиск) в статической копии не работают.

РЕЗУЛЬТАТ
<папка>\\site       — готовая копия (открывать через локальный веб-сервер)
<папка>\\mirror     — исходные файлы из архива
<папка>\\report.html — отчёт о покрытии

КОМАНДНАЯ СТРОКА
SiteRestorer.exe --cli --site <адрес> --out <папка> [--workers 16] [--limit N]
""",
    },
    "en": {
        "title": "Site Restorer — website copy from the web archive",
        "h1": "Restore a website from the web archive",
        "sub": "Paste an address — the app downloads the archived copy and builds it locally.",
        "site": "Site address",
        "site_ph": "e.g. example.com",
        "site_hint": "Paste a link from an e-mail, search results or the archive — the address is cleaned automatically.",
        "out": "Output folder",
        "browse": "Browse…",
        "start": "Start",
        "stop": "Stop",
        "open": "Open folder",
        "help": "Help",
        "lang": "Language",
        "log": "Log",
        "log_empty": "Detailed trace will appear here: requests, archive pauses, errors.",
        "stage": {
            "idle": "Ready",
            "inventory": "Archive inventory",
            "download": "Downloading files",
            "build": "Building offline copy",
            "verify": "Link check",
            "report": "Writing report",
            "done": "Done",
        },
        "step": "Step %d of 4",
        "t_html": "HTML pages", "t_code": "CSS / JS / XML", "t_img": "Images", "t_other": "Other files",
        "idle_info": "Waiting to start",
        "wait": "Preparing…",
        "stopping": "Stopping…",
        "speed": "speed", "threads": "threads", "left": "left", "ok": "ok",
        "noarchive": "not in archive", "errors": "errors", "pause": "pause",
        "done_msg": "Done!\n\nCopy: %s\nReport: %s",
        "err_title": "Error",
        "need_site": "Enter a site address, e.g. example.com",
        "need_out": "Choose an output folder.",
        "cancelled": "Stopped. Running again will resume.",
        "help_title": "Help — Site Restorer",
        "help_text": """WHAT IT DOES
Downloads every archived page and file of the given site from the Wayback Machine
(Internet Archive) and builds a static copy for local viewing.

ADDRESS FORMATS (paste anything)
example.com · https://www.example.com/page/ · EXAMPLE.COM
"example.com" · Site: example.com — copied from an e-mail
archive link: web.archive.org/web/2026.../https://example.com/
search link: google.com/url?q=https://example.com/
typed in a wrong keyboard layout: \\u0437\\u043a\\u0449\\u043b\\u0433\\u0437\\u0449\\u0434\\u044e\\u043a\\u0433
Cyrillic domain: \\u043f\\u0440\\u043e\\u043a\\u0443\\u043f\\u043e\\u043b.\\u0440\\u0444
The app extracts the host, drops tracking parameters, quotes and extra paths.

HOW TO USE
1. Enter a site address (e.g. example.com), with or without https://.
2. Choose an output folder.
3. Press Start. Stop interrupts the run; progress is kept and the next run resumes.

INDICATORS
• Big bar — overall progress across all stages.
• Stage caption — what is happening now (4 stages).
• Four bars — HTML, CSS/JS/XML, images, other files (done / total).
• Status line — speed, thread count, ETA, size, errors.
• Log — detailed trace including archive rate-limit pauses.

WHY IT TAKES TIME
The Internet Archive limits requests per IP. The app honours Retry-After, lowers
concurrency on 429 responses, raises it slowly when throughput grows, and pauses
automatically if limits persist. That is expected behaviour, not a failure.

WHAT IS RECOVERED
Only what the archive actually saved. Files the crawler never fetched (e.g. some
product images) are absent from every version and cannot be recovered.
Dynamic features (cart, login, search) do not work in a static copy.

OUTPUT
<folder>\\site        — ready copy (serve it with a local web server)
<folder>\\mirror      — raw archived files
<folder>\\report.html — coverage report

COMMAND LINE
SiteRestorer.exe --cli --site <address> --out <folder> [--workers 16] [--limit N]
""",
    },
}


def resource_path(name):
    base = getattr(sys, "_MEIPASS", os.path.dirname(os.path.abspath(__file__)))
    return os.path.join(base, name)


def load_cfg():
    try:
        with open(CFG_PATH, encoding="utf-8-sig") as f:
            return json.load(f)
    except Exception:
        return {}


def save_cfg(cfg):
    try:
        os.makedirs(CFG_DIR, exist_ok=True)
        with open(CFG_PATH, "w", encoding="utf-8") as f:
            json.dump(cfg, f, ensure_ascii=False, indent=1)
    except Exception:
        pass


def norm_path(p):
    """Windows-friendly path: backslashes, expanded ~, no trailing slash."""
    p = (p or "").strip().strip('"')
    p = os.path.expanduser(p)
    if not p:
        return p
    return os.path.normpath(p)


def human_bytes(n):
    for unit in ("Б", "КБ", "МБ", "ГБ"):
        if n < 1024 or unit == "ГБ":
            return ("%d %s" % (n, unit)) if unit == "Б" else ("%.1f %s" % (n, unit))
        n /= 1024.0


def human_time(sec):
    sec = int(sec)
    if sec < 60:
        return "%d с" % sec
    if sec < 3600:
        return "%d мин %02d с" % (sec // 60, sec % 60)
    return "%d ч %02d мин" % (sec // 3600, (sec % 3600) // 60)


def run_cli(args):
    try:
        import ctypes
        if sys.stdout is None:
            ctypes.windll.kernel32.AttachConsole(-1)
            sys.stdout = open("CONOUT$", "w", encoding="utf-8", errors="replace")
            sys.stderr = open("CONOUT$", "w", encoding="utf-8", errors="replace")
    except Exception:
        pass
    try:
        if sys.stdout is not None:
            sys.stdout.reconfigure(encoding="utf-8", errors="replace")
        if sys.stderr is not None:
            sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass
    if sys.stdout is None:
        class _Null:
            def write(self, *a):
                return 0

            def flush(self):
                pass
        sys.stdout = _Null()

    cancel = threading.Event()

    def emit(**kw):
        if kw.get("kind") == "log":
            print(kw.get("text", ""), flush=True)
        elif kw.get("kind") == "progress":
            st = kw.get("stage", "")
            extra = ""
            if st == "download":
                extra = " | %.0f KB/s thr=%d ok=%d miss=%d err=%d" % (
                    (kw.get("bps") or 0) / 1024.0, kw.get("threads", 0),
                    kw.get("ok", 0), kw.get("missing", 0), kw.get("errors", 0))
            print("[%5.1f%%] %-10s %s%s" % (kw.get("overall", 0), st,
                                            kw.get("text", ""), extra), flush=True)

    eng = Engine(args.site, args.out, workers=args.workers, rate=args.rate,
                 emit=emit, cancel=cancel, limit=args.limit, lang=args.lang)
    res = eng.run()
    print("RESULT %s" % json.dumps(res, ensure_ascii=False))


def run_gui():
    import tkinter as tk
    from tkinter import filedialog, messagebox, ttk
    from tkinter.scrolledtext import ScrolledText

    try:
        import ctypes
        ctypes.windll.shcore.SetProcessDpiAwareness(1)
    except Exception:
        pass

    cfg = load_cfg()
    lang = cfg.get("lang", "ru")
    if lang not in STR:
        lang = "ru"

    root = tk.Tk()
    root.title(STR[lang]["title"])
    root.geometry("880x740")
    root.minsize(840, 700)
    root.configure(bg=BG)
    try:
        root.iconbitmap(default=resource_path("app.ico"))
    except Exception:
        pass

    style = ttk.Style(root)
    try:
        style.theme_use("clam")
    except tk.TclError:
        pass
    style.configure("TEntry", fieldbackground="#ffffff", bordercolor="#d1d5db",
                    lightcolor="#d1d5db", darkcolor="#d1d5db", padding=(7, 5),
                    foreground=INK, insertcolor=INK)
    style.map("TEntry", bordercolor=[("focus", ACCENT)], lightcolor=[("focus", ACCENT)],
              darkcolor=[("focus", ACCENT)],
              fieldbackground=[("disabled", "#f3f4f6")],
              foreground=[("disabled", "#9ca3af")])
    style.configure("TCombobox", fieldbackground="#ffffff", background="#ffffff",
                    bordercolor="#d1d5db", arrowcolor="#4b5563", padding=(6, 4))
    style.map("TCombobox", fieldbackground=[("readonly", "#ffffff")],
              bordercolor=[("focus", ACCENT)])
    style.configure("TScrollbar", background="#d1d5db", troughcolor="#f3f4f6",
                    bordercolor="#f3f4f6", arrowcolor="#6b7280")
    style.configure("Dark.Vertical.TScrollbar", background="#374151", troughcolor="#111827",
                    bordercolor="#111827", arrowcolor="#9ca3af", gripcount=0)
    style.map("Dark.Vertical.TScrollbar", background=[("active", "#4b5563")])
    style.configure("TLabel", background=BG, foreground=INK, font=(FONT, 9))
    style.configure("Card.TLabel", background=CARD, foreground=INK, font=(FONT, 9))
    style.configure("Muted.TLabel", background=BG, foreground=MUTED, font=(FONT, 9))
    style.configure("CardMuted.TLabel", background=CARD, foreground=MUTED, font=(FONT, 9))
    style.configure("H1.TLabel", background=BG, foreground=INK, font=(FONT, 16, "bold"))
    style.configure("Section.TLabel", background=CARD, foreground=INK, font=(FONT, 10, "bold"))
    style.configure("Bar.Horizontal.TProgressbar", troughcolor="#dde3ec", bordercolor="#dde3ec",
                    background=ACCENT, lightcolor=ACCENT, darkcolor=ACCENT, thickness=14,
                    borderwidth=0)
    style.configure("Thin.Horizontal.TProgressbar", troughcolor="#e8edf4", bordercolor="#e8edf4",
                    background=ACCENT, lightcolor=ACCENT, darkcolor=ACCENT, thickness=8,
                    borderwidth=0)
    style.configure("TEntry", fieldbackground="#ffffff")

    q = queue.Queue()
    state = {"running": False, "cancel": threading.Event(), "stage": "idle", "stage_t0": 0.0, "pulse": 0, "step": "", "step_t0": 0.0, "step_key": ""}
    L = {"lang": lang}

    def t(key):
        return STR[L["lang"]][key]

    def make_accent(parent, text, command):
        return tk.Button(parent, text=text, command=command, bg=ACCENT, fg="#ffffff",
                         activebackground=ACCENT_D, activeforeground="#ffffff",
                         disabledforeground="#dbe3f0", relief="flat", bd=0,
                         font=(FONT, 9, "bold"), padx=18, pady=7, cursor="hand2")

    def ghost_state(btn, enabled):
        if enabled:
            btn.configure(state="normal", bg="#ffffff", fg=INK, relief="solid",
                          highlightbackground=LINE, cursor="hand2")
        else:
            btn.configure(state="disabled", bg="#f6f7f9", fg="#9ca3af",
                          disabledforeground="#9ca3af", relief="solid", cursor="arrow")

    def make_ghost(parent, text, command=None, width=0):
        b = tk.Button(parent, text=text, command=command, bg="#ffffff", fg=INK,
                      activebackground="#f3f4f6", activeforeground=INK,
                      disabledforeground="#9ca3af", relief="solid", bd=1,
                      highlightthickness=0, font=(FONT, 9), padx=14, pady=6,
                      cursor="hand2", width=width)
        b.bind("<Enter>", lambda e: b.configure(bg="#f8fafc") if str(b["state"]) == "normal" else None)
        b.bind("<Leave>", lambda e: b.configure(bg="#ffffff") if str(b["state"]) == "normal" else None)
        return b

    # ---------------- header
    header = tk.Frame(root, bg=BG)
    header.pack(fill="x", padx=24, pady=(18, 6))
    left = tk.Frame(header, bg=BG)
    left.pack(side="left", fill="x", expand=True)
    h1 = ttk.Label(left, text=t("h1"), style="H1.TLabel")
    h1.pack(anchor="w")
    sub = ttk.Label(left, text=t("sub"), style="Muted.TLabel", wraplength=600, justify="left")
    sub.pack(anchor="w", pady=(4, 0))

    right = tk.Frame(header, bg=BG)
    right.pack(side="right", anchor="ne")
    lang_lbl = ttk.Label(right, text=t("lang"), style="Muted.TLabel")
    lang_lbl.pack(side="left", padx=(0, 8))
    lang_var = tk.StringVar(value="Русский" if lang == "ru" else "English")
    lang_box = ttk.Combobox(right, textvariable=lang_var, values=["Русский", "English"],
                            state="readonly", width=10, font=(FONT, 9))
    lang_box.pack(side="left")

    # ---------------- form
    form = tk.Frame(root, bg=BG)
    form.pack(fill="x", padx=24, pady=(10, 0))
    form.columnconfigure(1, weight=1)

    site_lbl = ttk.Label(form, text=t("site"), style="TLabel", anchor="e", width=16)
    site_lbl.grid(row=0, column=0, sticky="e", padx=(0, 12), pady=(0, 2))
    site_var = tk.StringVar(value=cfg.get("last_site", ""))
    site_entry = ttk.Entry(form, textvariable=site_var, font=(FONT, 11))
    site_entry.grid(row=0, column=1, columnspan=2, sticky="ew", pady=(0, 2), ipady=4)

    site_hint = tk.Label(form, text=t("site_hint"), bg=BG, fg=MUTED, anchor="w",
                         font=(FONT, 9))
    site_hint.grid(row=1, column=1, columnspan=2, sticky="w", pady=(0, 10))

    out_lbl = ttk.Label(form, text=t("out"), style="TLabel", anchor="e", width=16)
    out_lbl.grid(row=2, column=0, sticky="e", padx=(0, 12))
    out_var = tk.StringVar(value=norm_path(cfg.get("last_out") or os.path.join(
        os.path.expanduser("~"), "Documents", "SiteRestore", "site")))
    out_entry = ttk.Entry(form, textvariable=out_var, font=(FONT, 10))
    out_entry.grid(row=2, column=1, sticky="ew", ipady=4)
    browse_btn = make_ghost(form, t("browse"))
    browse_btn.grid(row=2, column=2, sticky="e", padx=(8, 0))

    # ---------------- actions
    actions = tk.Frame(root, bg=BG)
    actions.pack(fill="x", padx=24, pady=(16, 12))
    start_btn = make_accent(actions, t("start"), None)
    start_btn.pack(side="left")
    stop_btn = make_ghost(actions, t("stop"))
    stop_btn.pack(side="left", padx=8)
    open_btn = make_ghost(actions, t("open"))
    open_btn.pack(side="left")
    help_btn = make_ghost(actions, t("help"))
    help_btn.pack(side="right")

    # ---------------- progress card
    card = tk.Frame(root, bg=CARD, highlightbackground=LINE, highlightthickness=1)
    card.pack(fill="x", padx=24)
    inner = tk.Frame(card, bg=CARD)
    inner.pack(fill="x", padx=16, pady=14)

    top_row = tk.Frame(inner, bg=CARD)
    top_row.pack(fill="x")
    stage_lbl = ttk.Label(top_row, text=t("stage")["idle"], style="Section.TLabel")
    stage_lbl.pack(side="left")
    pct_lbl = ttk.Label(top_row, text="0 %", style="Card.TLabel", font=(FONT, 10, "bold"))
    pct_lbl.pack(side="right", padx=(0, 17))

    overall = ttk.Progressbar(inner, style="Bar.Horizontal.TProgressbar", maximum=100)
    overall.pack(fill="x", pady=(8, 12))

    type_rows = {}
    for key, label_key in (("html", "t_html"), ("code", "t_code"),
                           ("img", "t_img"), ("other", "t_other")):
        row = tk.Frame(inner, bg=CARD)
        row.pack(fill="x", pady=2)
        lbl = ttk.Label(row, text=t(label_key), style="CardMuted.TLabel", width=16, anchor="w")
        lbl.pack(side="left")
        bar = ttk.Progressbar(row, style="Thin.Horizontal.TProgressbar", maximum=100)
        bar.pack(side="left", fill="x", expand=True, padx=(0, 10))
        cnt = ttk.Label(row, text="0 / 0", style="CardMuted.TLabel", width=11, anchor="e")
        cnt.pack(side="right")
        type_rows[key] = (bar, cnt, lbl)

    sep = tk.Frame(inner, bg=LINE, height=1)
    sep.pack(fill="x", pady=(12, 10))

    status1 = ttk.Label(inner, text=t("idle_info"), style="Card.TLabel")
    status1.pack(anchor="w")
    status2 = ttk.Label(inner, text="", style="CardMuted.TLabel")
    status2.pack(anchor="w", pady=(2, 0))

    # ---------------- log
    log_head = tk.Frame(root, bg=BG)
    log_head.pack(fill="x", padx=24, pady=(16, 6))
    log_lbl = ttk.Label(log_head, text=t("log"), style="TLabel", font=(FONT, 9, "bold"))
    log_lbl.pack(side="left")

    log_frame = tk.Frame(root, bg=BG)
    log_frame.pack(fill="both", expand=True, padx=24, pady=(0, 18))
    log_txt = tk.Text(log_frame, height=7, wrap="word", font=("Consolas", 9),
                      bg="#111827", fg="#d1d5db", insertbackground="#d1d5db",
                      borderwidth=0, padx=12, pady=10)
    sb = ttk.Scrollbar(log_frame, command=log_txt.yview, style="Dark.Vertical.TScrollbar")
    log_txt.configure(yscrollcommand=sb.set)
    log_txt.pack(side="left", fill="both", expand=True)
    sb.pack(side="right", fill="y")
    log_txt.configure(state="normal")
    log_txt.insert("1.0", t("log_empty"))
    log_txt.tag_add("empty", "1.0", "end")
    log_txt.tag_config("empty", foreground="#9ca3af")
    log_txt.configure(state="disabled")

    log_state = {"has_content": False}

    def log_line(text, level="info"):
        color = {"info": "#d1d5db", "warn": "#fbbf24", "err": "#f87171", "ok": "#4ade80"}.get(level, "#d1d5db")
        log_txt.configure(state="normal")
        if not log_state["has_content"]:
            log_txt.delete("1.0", "end")
            log_state["has_content"] = True
        log_txt.insert("end", "%s  %s\n" % (time.strftime("%H:%M:%S"), text), level)
        log_txt.tag_config(level, foreground=color)
        log_txt.see("end")
        log_txt.configure(state="disabled")

    def clear_log():
        log_txt.configure(state="normal")
        log_txt.delete("1.0", "end")
        log_txt.configure(state="disabled")
        log_state["has_content"] = False

    # ---------------- language
    def apply_lang(new_lang):
        L["lang"] = new_lang
        cfg["lang"] = new_lang
        save_cfg(cfg)
        S = STR[new_lang]
        root.title(S["title"])
        h1.configure(text=S["h1"])
        sub.configure(text=S["sub"])
        lang_lbl.configure(text=S["lang"])
        site_lbl.configure(text=S["site"])
        site_hint.configure(text=S["site_hint"])
        out_lbl.configure(text=S["out"])
        browse_btn.configure(text=S["browse"])
        start_btn.configure(text=S["start"])
        stop_btn.configure(text=S["stop"])
        open_btn.configure(text=S["open"])
        help_btn.configure(text=S["help"])
        log_lbl.configure(text=S["log"])
        for key, (_bar, _cnt, lbl) in type_rows.items():
            lbl.configure(text=S["t_" + key])
        if not state["running"]:
            stage_lbl.configure(text=S["stage"]["idle"])
            status1.configure(text=S["idle_info"])
            status2.configure(text="")

    def on_lang(_evt=None):
        apply_lang("ru" if lang_var.get() == "Русский" else "en")

    lang_box.bind("<<ComboboxSelected>>", on_lang)

    # ---------------- help
    def show_help():
        win = tk.Toplevel(root)
        win.title(STR[L["lang"]]["help_title"])
        win.geometry("780x620")
        win.configure(bg=CARD)
        txt = ScrolledText(win, wrap="word", font=(FONT, 9), padx=16, pady=14,
                           bg=CARD, fg=INK, borderwidth=0)
        txt.pack(fill="both", expand=True)
        txt.insert("1.0", STR[L["lang"]]["help_text"])
        txt.configure(state="disabled")
        make_accent(win, "OK", win.destroy).pack(pady=10)

    help_btn.configure(command=show_help)

    # ---------------- normalization
    def normalize_field(_evt=None):
        raw = site_var.get().strip()
        if not raw:
            return
        host, _note = sanitize_site(raw)
        if host and host != raw:
            site_var.set(host)
            site_hint.configure(text="→ " + host, fg=OK)
        elif host:
            site_hint.configure(text=t("site_hint"), fg=MUTED)
        else:
            site_hint.configure(text=("⚠ проверьте адрес" if L["lang"] == "ru"
                                      else "⚠ check the address"), fg=WARN)

    def paste_normalize(_evt=None):
        root.after(60, normalize_field)
        return None

    site_entry.bind("<<Paste>>", paste_normalize, add="+")
    site_entry.bind("<FocusOut>", normalize_field, add="+")
    site_entry.bind("<Return>", lambda e: (normalize_field(), start()))

    def browse():
        d = filedialog.askdirectory(initialdir=os.path.dirname(out_var.get()) or os.path.expanduser("~"))
        if d:
            out_var.set(norm_path(os.path.join(d, "site")))

    browse_btn.configure(command=browse)

    # ---------------- worker
    def worker(site, out):
        cancel = state["cancel"]

        def emit(**kw):
            q.put(kw)

        try:
            eng = Engine(site, out, workers=16, rate=0.0, emit=emit, cancel=cancel, lang=L["lang"])
            res = eng.run()
            q.put({"kind": "done", "res": res})
        except Cancelled:
            q.put({"kind": "cancelled"})
        except Exception as e:
            q.put({"kind": "failed", "text": "%s\n%s" % (e, traceback.format_exc(limit=3))})

    def start():
        raw = site_var.get()
        site, _note = sanitize_site(raw)
        if not site:
            messagebox.showwarning(STR[L["lang"]]["title"], STR[L["lang"]]["need_site"])
            return
        if site != raw.strip():
            site_var.set(site)
            log_line("URL: %r -> %s" % (raw.strip()[:80], site), "info")
        out = norm_path(out_var.get())
        if not out:
            messagebox.showwarning(STR[L["lang"]]["title"], STR[L["lang"]]["need_out"])
            return
        out_var.set(out)
        os.makedirs(out, exist_ok=True)
        cfg["last_site"] = site
        cfg["last_out"] = out
        save_cfg(cfg)
        state["running"] = True
        state["cancel"].clear()
        start_btn.configure(state="disabled", bg="#e2e8f0", disabledforeground="#94a3b8")
        ghost_state(stop_btn, True)
        ghost_state(open_btn, False)
        site_entry.configure(state="disabled")
        out_entry.configure(state="disabled")
        overall["value"] = 0
        pct_lbl.configure(text="0 %")
        for bar, cnt, _lbl in type_rows.values():
            bar["value"] = 0
            cnt.configure(text="0 / 0")
        stage_lbl.configure(text=STR[L["lang"]]["stage"]["inventory"])
        state["step"] = ""
        state["step_t0"] = 0.0
        state["step_key"] = ""
        status1.configure(text=STR[L["lang"]]["wait"])
        status2.configure(text="")
        clear_log()
        threading.Thread(target=worker, args=(site, out), daemon=True).start()

    def stop():
        state["cancel"].set()
        status1.configure(text=STR[L["lang"]]["stopping"])

    def open_folder():
        path = norm_path(out_var.get())
        if path and os.path.isdir(path):
            os.startfile(path)

    ghost_state(stop_btn, False)
    ghost_state(open_btn, False)
    start_btn.configure(command=start)
    stop_btn.configure(command=stop)
    open_btn.configure(command=open_folder)

    def finish_run():
        state["running"] = False
        start_btn.configure(state="normal", bg=ACCENT)
        ghost_state(stop_btn, False)
        site_entry.configure(state="normal")
        out_entry.configure(state="normal")

    def poll():
        S = STR[L["lang"]]
        stage_txt = {"idle": S["stage"]["idle"], "inventory": "1 · " + S["stage"]["inventory"],
                     "download": "2 · " + S["stage"]["download"], "build": "3 · " + S["stage"]["build"],
                     "verify": "4 · " + S["stage"]["verify"], "report": S["stage"]["report"],
                     "done": S["stage"]["done"]}
        try:
            while True:
                ev = q.get_nowait()
                kind = ev.get("kind")
                if kind == "log":
                    log_line(ev.get("text", ""), ev.get("level", "info"))
                elif kind == "step":
                    key = ev.get("key") or ev.get("title") or ""
                    if key != state.get("step_key"):
                        state["step_key"] = key
                        state["step_t0"] = time.time()
                    title = ev.get("title") or ""
                    detail = ev.get("text") or ""
                    if title:
                        status1.configure(text=title)
                    state["step"] = detail
                    status2.configure(text=detail)
                elif kind == "progress":
                    st_now = ev.get("stage", "")
                    if st_now != state["stage"]:
                        state["stage"] = st_now
                        state["stage_t0"] = time.time()
                        state["pulse"] = 0
                        if st_now != "inventory":
                            state["step"] = ""
                    ov = ev.get("overall", 0)
                    overall["value"] = ov
                    pct_lbl.configure(text="%.0f %%" % ov)
                    stage_lbl.configure(text=stage_txt.get(ev.get("stage", ""), "…"))
                    for key, (bar, cnt, _lbl) in type_rows.items():
                        ok, tot = (ev.get("types", {}).get(key) or [0, 0])[:2]
                        bar["value"] = 100.0 * ok / tot if tot else 0
                        cnt.configure(text="%d / %d" % (ok, tot))
                    st_name = ev.get("stage", "")
                    defaults = {
                        "inventory": ("Запрос к индексу архива…" if L["lang"] == "ru" else "Querying the archive index…"),
                        "download": ("Подготовка загрузки…" if L["lang"] == "ru" else "Preparing download…"),
                        "build": ("Сборка файлов…" if L["lang"] == "ru" else "Building files…"),
                        "verify": ("Проверка ссылок…" if L["lang"] == "ru" else "Checking links…"),
                        "report": ("Формирование отчёта…" if L["lang"] == "ru" else "Writing report…"),
                    }
                    txt = ev.get("text") or defaults.get(st_name, "")
                    if st_name == "inventory" and txt and "/" in txt:
                        txt = ("Страница CDX " if L["lang"] == "ru" else "CDX page ") + txt
                    status1.configure(text=txt)
                    if ev.get("stage") == "download":
                        parts = ["%s %s/с" % (S["speed"], human_bytes(ev.get("bps") or 0)),
                                 "%s %d" % (S["threads"], ev.get("threads", 0)),
                                 ("%.1f" % ev.get("speed", 0)).replace(".", ",") + " " +
                                 ("файлов/с" if L["lang"] == "ru" else "files/s")]
                        if ev.get("eta"):
                            parts.append("%s ~%s" % (S["left"], human_time(ev["eta"])))
                        parts.append(human_bytes(ev.get("bytes", 0)))
                        if ev.get("missing"):
                            parts.append("%s %d" % (S["noarchive"], ev["missing"]))
                        if ev.get("errors"):
                            parts.append("%s %d" % (S["errors"], ev["errors"]))
                        if ev.get("pause"):
                            parts.append("%s %dс" % (S["pause"], ev["pause"]))
                        status2.configure(text="   ·   ".join(parts))
                elif kind == "done":
                    res = ev.get("res", {})
                    finish_run()
                    stage_lbl.configure(text=S["stage"]["done"])
                    overall["value"] = 100
                    pct_lbl.configure(text="100 %")
                    status1.configure(text="%d / %d" % (res.get("ok", 0), res.get("total", 0)))
                    status2.configure(text=res.get("site", ""))
                    ghost_state(open_btn, True)
                    log_line("Готово: %s" % res.get("site", ""), "ok")
                    messagebox.showinfo(S["title"], S["done_msg"] % (res.get("site", ""),
                                                                     res.get("report", "")))
                elif kind == "cancelled":
                    finish_run()
                    stage_lbl.configure(text=S["cancelled"])
                    status1.configure(text=S["cancelled"])
                    status2.configure(text="")
                elif kind == "failed":
                    finish_run()
                    txt = ev.get("text", "")
                    if "archive unavailable" in txt:
                        txt = ("Архив временно недоступен (HTTP 503/429). Подождите 5–10 минут и запустите снова — "
                               "прогресс сохранится." if L["lang"] == "ru" else
                               "The archive is temporarily unavailable (HTTP 503/429). Wait 5–10 minutes and retry — "
                               "progress is kept.")
                    elif "archive has no pages" in txt:
                        txt = ("В архиве нет страниц по этому адресу. Проверьте написание домена."
                               if L["lang"] == "ru" else
                               "The archive has no pages for this address. Check the domain spelling.")
                    stage_lbl.configure(text=S["err_title"])
                    status1.configure(text=S["err_title"])
                    status2.configure(text="")
                    log_line(ev.get("text", ""), "err")
                    messagebox.showerror(S["title"], ev.get("text", "")[:800])
        except queue.Empty:
            pass
        root.after(120, poll)

    def pulse_tick():
        if state["running"] and state["stage"] == "inventory":
            state["pulse"] += 1
            overall["value"] = min(8.0, 2.0 + (state["pulse"] % 13) * 0.5)
            if state.get("step"):
                el = int(time.time() - (state["step_t0"] or time.time()))
                sep = " · %d с" % el if L["lang"] == "ru" else " · %d s" % el
                status2.configure(text=state["step"] + sep)
        root.after(250, pulse_tick)

    pulse_tick()
    poll()
    site_entry.focus_set()
    root.mainloop()


def main():
    ap = argparse.ArgumentParser(add_help=True)
    ap.add_argument("--cli", action="store_true")
    ap.add_argument("--site", default="")
    ap.add_argument("--out", default="")
    ap.add_argument("--workers", type=int, default=16)
    ap.add_argument("--rate", type=float, default=0.0)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--lang", default="ru", choices=["ru", "en"])
    args, _ = ap.parse_known_args()
    if args.cli:
        args.out = norm_path(args.out)
        os.makedirs(args.out, exist_ok=True)
        run_cli(args)
    else:
        run_gui()


if __name__ == "__main__":
    main()
