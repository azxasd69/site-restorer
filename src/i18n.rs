//! Строки интерфейса: RU и EN.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ru,
    En,
}

pub struct T {
    pub title: &'static str,
    pub heading: &'static str,
    pub subtitle: &'static str,
    pub site: &'static str,
    pub site_ph: &'static str,
    pub out: &'static str,
    pub browse: &'static str,
    pub analyse: &'static str,
    pub start: &'static str,
    pub stop: &'static str,
    pub open: &'static str,
    pub help: &'static str,
    pub log: &'static str,
    pub stage_idle: &'static str,
    pub stage_inventory: &'static str,
    pub stage_download: &'static str,
    pub stage_build: &'static str,
    pub stage_verify: &'static str,
    pub stage_report: &'static str,
    pub stage_done: &'static str,
    pub waiting: &'static str,
    pub preparing: &'static str,
    pub stopped: &'static str,
    pub error: &'static str,
    pub built: &'static str,
    pub t_html: &'static str,
    pub t_code: &'static str,
    pub t_img: &'static str,
    pub t_other: &'static str,
    pub hint: &'static str,
    pub bad_address: &'static str,
    pub need_out: &'static str,
    pub help_text: &'static str,
    pub lang_hint: &'static str,
    pub k_loaded: &'static str,
    pub k_speed: &'static str,
    pub k_threads: &'static str,
    pub k_issues: &'static str,
    pub empty_title: &'static str,
    pub empty_note: &'static str,
    pub s_inv: &'static str,
    pub s_dl: &'static str,
    pub s_build: &'static str,
    pub s_verify: &'static str,
    pub s_report: &'static str,
    pub eta_hint: &'static str,
}

pub static RU: T = T {
    title: "Site Restorer — копия сайта из веб-архива",
    heading: "Восстановление сайта из веб-архива",
    subtitle: "Скачивает сохранённую копию сайта из Wayback Machine и собирает её для локального просмотра.",
    site: "Адрес сайта",
    site_ph: "например, prokupol.ru",
    out: "Папка для копии",
    browse: "Обзор…",
    analyse: "Разобрать",
    start: "Начать",
    stop: "Остановить",
    open: "Открыть папку",
    help: "Справка",
    log: "Журнал",
    stage_idle: "Готов к запуску",
    stage_inventory: "Инвентаризация архива",
    stage_download: "Скачивание файлов",
    stage_build: "Сборка оффлайн-копии",
    stage_verify: "Проверка ссылок",
    stage_report: "Отчёт",
    stage_done: "Готово",
    waiting: "Ожидание запуска",
    preparing: "Подготовка…",
    stopped: "Остановлено",
    error: "Ошибка",
    built: "Копия собрана",
    t_html: "HTML-страницы",
    t_code: "CSS / JS / XML",
    t_img: "Изображения",
    t_other: "Прочие файлы",
    hint: "Вставьте адрес (Ctrl+V) — можно ссылку из письма, поиска или архива.",
    bad_address: "⚠ Не разобрал адрес — проверьте написание домена.",
    need_out: "Укажите папку для копии.",
    lang_hint: "Переключить язык",
    k_loaded: "Скачано",
    k_speed: "Скорость",
    k_threads: "Потоки",
    k_issues: "Битые / ошибки",
    empty_title: "Пока ничего не скачано",
    empty_note: "Нажмите «Начать»: программа проверит архив, скачает файлы и соберёт копию.",
    s_inv: "Инвентаризация",
    s_dl: "Скачивание",
    s_build: "Сборка",
    s_verify: "Проверка",
    s_report: "Отчёт",
    eta_hint: "осталось",
    help_text: "ЧТО ДЕЛАЕТ ПРОГРАММА\n\
Скачивает из Wayback Machine (Internet Archive) все сохранённые страницы и файлы\n\
указанного сайта и собирает статическую копию для локального просмотра.\n\n\
ФОРМАТЫ АДРЕСА (можно вставлять как угодно)\n\
prokupol.ru · https://www.prokupol.ru/catalog/ · PROKUPOL.RU\n\
«prokupol.ru» · Сайт: prokupol.ru — ссылка из письма\n\
ссылка из архива: web.archive.org/web/2026.../https://prokupol.ru/\n\
ссылка из поиска: google.com/url?q=https://prokupol.ru/\n\
адрес, набранный в русской раскладке: зкщлгзщдюкг\n\
кириллический домен: прокупол.рф\n\n\
ЧТО ВОССТАНАВЛИВАЕТСЯ\n\
Только то, что архив когда-либо сохранил. Файлы, которые краулер не забирал,\n\
отсутствуют — их нет ни в одной версии. Динамика (корзина, вход, поиск)\n\
в статической копии не работает.\n\n\
РЕЗУЛЬТАТ\n\
<папка>/site        — готовая копия (открывать локальным веб-сервером)\n\
<папка>/mirror      — исходные файлы из архива\n\
<папка>/report.html — отчёт о покрытии\n\n\
КОМАНДНАЯ СТРОКА\n\
site-restorer --cli --site <адрес> --out <папка> [--limit N] [--workers N]\n",
};

pub static EN: T = T {
    title: "Site Restorer — a website copy from the web archive",
    heading: "Restore a website from the web archive",
    subtitle: "Downloads the archived copy of a site from the Wayback Machine and builds it for local viewing.",
    site: "Site address",
    site_ph: "e.g. example.com",
    out: "Output folder",
    browse: "Browse…",
    analyse: "Parse",
    start: "Start",
    stop: "Stop",
    open: "Open folder",
    help: "Help",
    log: "Log",
    stage_idle: "Ready",
    stage_inventory: "Archive inventory",
    stage_download: "Downloading files",
    stage_build: "Building offline copy",
    stage_verify: "Checking links",
    stage_report: "Report",
    stage_done: "Done",
    waiting: "Waiting to start",
    preparing: "Preparing…",
    stopped: "Stopped",
    error: "Error",
    built: "Copy is ready",
    t_html: "HTML pages",
    t_code: "CSS / JS / XML",
    t_img: "Images",
    t_other: "Other files",
    hint: "Paste an address (Ctrl+V) — a link from an e-mail, search results or the archive.",
    bad_address: "⚠ Could not parse the address — check the domain spelling.",
    need_out: "Choose an output folder.",
    lang_hint: "Switch language",
    k_loaded: "Downloaded",
    k_speed: "Speed",
    k_threads: "Threads",
    k_issues: "Broken / errors",
    empty_title: "Nothing downloaded yet",
    empty_note: "Press Start: the program checks the archive, downloads files and builds the copy.",
    s_inv: "Inventory",
    s_dl: "Download",
    s_build: "Build",
    s_verify: "Check",
    s_report: "Report",
    eta_hint: "left",
    help_text: "WHAT IT DOES\n\
Downloads every archived page and file of a site from the Wayback Machine\n\
(Internet Archive) and builds a static copy for local viewing.\n\n\
ADDRESS FORMATS (paste anything)\n\
example.com · https://www.example.com/page/ · EXAMPLE.COM\n\
\"example.com\" · Site: example.com — copied from an e-mail\n\
archive link: web.archive.org/web/2026.../https://example.com/\n\
search link: google.com/url?q=https://example.com/\n\
typed in the wrong keyboard layout: зкщлгзщдюкг\n\
Cyrillic domain: прокупол.рф\n\n\
WHAT IS RECOVERED\n\
Only what the archive actually saved. Files the crawler never fetched are\n\
absent from every version. Dynamic features (cart, login, search) do not work.\n\n\
OUTPUT\n\
<folder>/site        — ready copy (serve it with a local web server)\n\
<folder>/mirror      — raw archived files\n\
<folder>/report.html — coverage report\n\n\
COMMAND LINE\n\
site-restorer --cli --site <address> --out <folder> [--limit N] [--workers N]\n",
};

impl Lang {
    pub fn t(self) -> &'static T {
        match self {
            Lang::Ru => &RU,
            Lang::En => &EN,
        }
    }

    pub fn other(self) -> Lang {
        match self {
            Lang::Ru => Lang::En,
            Lang::En => Lang::Ru,
        }
    }
}
