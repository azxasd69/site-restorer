/* Site Restorer — уведомление о cookie и Яндекс.Метрика.
   Режим «только уведомление»: счётчик загружается сразу, баннер сообщает об этом
   и даёт отключить статистику (выбор хранится в localStorage по ключу pf-consent).
   Значения: 'notified' — посетитель прочитал уведомление, 'denied' — отказался. */
(function () {
  'use strict';

  var STORAGE_KEY = 'pf-consent';
  var COUNTER_ID = 114170389;
  var loaded = false;

  var TEXT = {
    ru: {
      text: 'Сайт использует cookie и Яндекс.Метрику для статистики посещений. Счётчик уже работает — вы можете его отключить.',
      accept: 'Понятно',
      deny: 'Отключить статистику',
      more: 'Политика конфиденциальности',
      moreHref: '/privacy/',
      label: 'Уведомление об использовании cookie'
    },
    en: {
      text: 'This site uses cookies and Yandex Metrica for visit statistics. The counter is already running — you can turn it off.',
      accept: 'Got it',
      deny: 'Turn off statistics',
      more: 'Privacy policy',
      moreHref: '/en/privacy/',
      label: 'Cookie notice'
    }
  };

  function currentLang() {
    var lang = (document.documentElement.getAttribute('lang') || 'ru').toLowerCase();
    return lang.indexOf('en') === 0 ? 'en' : 'ru';
  }

  function readChoice() {
    try {
      return window.localStorage.getItem(STORAGE_KEY);
    } catch (error) {
      return null;
    }
  }

  function saveChoice(value) {
    try {
      window.localStorage.setItem(STORAGE_KEY, value);
    } catch (error) {
      /* приватный режим — выбор не сохранится */
    }
  }

  function dropMetrikaCookies() {
    var cookies = document.cookie ? document.cookie.split(';') : [];
    var hosts = ['', '; path=/', '; path=/; domain=' + location.hostname, '; path=/; domain=.' + location.hostname];
    cookies.forEach(function (item) {
      var name = item.split('=')[0].trim();
      if (!name) return;
      if (name.indexOf('_ym') === 0 || name.indexOf('ymex') === 0) {
        hosts.forEach(function (scope) {
          document.cookie = name + '=; expires=Thu, 01 Jan 1970 00:00:00 GMT' + scope;
        });
      }
    });
  }

  function loadMetrika() {
    if (loaded) return;
    loaded = true;
    (function (m, e, t, r, i, k, a) {
      m[i] = m[i] || function () { (m[i].a = m[i].a || []).push(arguments); };
      m[i].l = 1 * new Date();
      for (var j = 0; j < document.scripts.length; j++) {
        if (document.scripts[j].src === r) { return; }
      }
      k = e.createElement(t); a = e.getElementsByTagName(t)[0];
      k.async = 1; k.src = r; a.parentNode.insertBefore(k, a);
    })(window, document, 'script', 'https://mc.yandex.ru/metrika/tag.js?id=' + COUNTER_ID, 'ym');

    window.ym(COUNTER_ID, 'init', {
      ssr: true,
      webvisor: true,
      clickmap: true,
      ecommerce: 'dataLayer',
      referrer: document.referrer,
      url: location.href,
      accurateTrackBounce: true,
      trackLinks: true
    });
  }

  function removeBar() {
    var bar = document.querySelector('[data-cookie-bar]');
    if (bar && bar.parentNode) bar.parentNode.removeChild(bar);
  }

  function showBar() {
    if (document.querySelector('[data-cookie-bar]')) return;
    var t = TEXT[currentLang()];

    var bar = document.createElement('div');
    bar.className = 'cookie-bar';
    bar.setAttribute('data-cookie-bar', '');
    bar.setAttribute('role', 'region');
    bar.setAttribute('aria-label', t.label);
    bar.setAttribute('aria-live', 'polite');

    var inner = document.createElement('div');
    inner.className = 'cookie-bar-inner';

    var p = document.createElement('p');
    p.className = 'cookie-bar-text';
    p.textContent = t.text;

    var link = document.createElement('a');
    link.href = t.moreHref;
    link.textContent = t.more;
    link.className = 'cookie-bar-link';
    p.appendChild(document.createTextNode(' '));
    p.appendChild(link);

    var actions = document.createElement('div');
    actions.className = 'cookie-bar-actions';

    var deny = document.createElement('button');
    deny.type = 'button';
    deny.className = 'btn btn-secondary btn-small';
    deny.textContent = t.deny;
    deny.addEventListener('click', function () {
      saveChoice('denied');
      // сначала отправляем цель, потом чистим cookie и перезагружаемся
      try {
        if (typeof window.ym === 'function') window.ym(COUNTER_ID, 'reachGoal', 'consent_off');
      } catch (error) {
        /* счётчик мог не загрузиться */
      }
      window.setTimeout(function () {
        dropMetrikaCookies();
        window.location.reload();
      }, 350);
    });

    var accept = document.createElement('button');
    accept.type = 'button';
    accept.className = 'btn btn-primary btn-small';
    accept.textContent = t.accept;
    accept.addEventListener('click', function () {
      saveChoice('notified');
      removeBar();
    });

    actions.appendChild(deny);
    actions.appendChild(accept);
    inner.appendChild(p);
    inner.appendChild(actions);
    bar.appendChild(inner);
    document.body.appendChild(bar);
  }

  document.querySelectorAll('[data-consent-open]').forEach(function (el) {
    el.addEventListener('click', function (event) {
      event.preventDefault();
      showBar();
    });
  });

  var choice = readChoice();

  // Режим «только уведомление»: считаем всех, кроме тех, кто отказался.
  if (choice !== 'denied') {
    loadMetrika();
  }

  // Уведомление показываем один раз — пока посетитель не нажмёт кнопку.
  if (!choice) {
    showBar();
  }

  window.pfConsent = {
    open: showBar,
    load: loadMetrika,
    choice: readChoice
  };
})();
