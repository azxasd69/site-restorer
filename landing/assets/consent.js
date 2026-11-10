/* Site Restorer — согласие на cookie и загрузка Яндекс.Метрики.
   Счётчик не загружается, пока посетитель не согласился; выбор хранится
   в localStorage по ключу pf-consent. Без JavaScript счётчик не работает. */
(function () {
  'use strict';

  var STORAGE_KEY = 'pf-consent';
  var COUNTER_ID = 114170389;
  var loaded = false;

  var TEXT = {
    ru: {
      text: 'Сайт использует cookie и Яндекс.Метрику, чтобы считать посещения. Пока вы не согласитесь, счётчик не загружается.',
      accept: 'Принять',
      deny: 'Только необходимые',
      more: 'Политика конфиденциальности',
      moreHref: '/privacy/',
      label: 'Согласие на использование cookie'
    },
    en: {
      text: 'This site uses cookies and Yandex Metrica to count visits. The counter stays off until you agree.',
      accept: 'Accept',
      deny: 'Essential only',
      more: 'Privacy policy',
      moreHref: '/en/privacy/',
      label: 'Cookie consent'
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
      /* приватный режим — выбор не сохранится, баннер покажется снова */
    }
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
    bar.setAttribute('role', 'dialog');
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
      removeBar();
    });

    var accept = document.createElement('button');
    accept.type = 'button';
    accept.className = 'btn btn-primary btn-small';
    accept.textContent = t.accept;
    accept.addEventListener('click', function () {
      saveChoice('granted');
      loadMetrika();
      removeBar();
    });

    actions.appendChild(deny);
    actions.appendChild(accept);
    inner.appendChild(p);
    inner.appendChild(actions);
    bar.appendChild(inner);
    document.body.appendChild(bar);
  }

  // Ссылки «Настройки cookie» в подвале
  document.querySelectorAll('[data-consent-open]').forEach(function (el) {
    el.addEventListener('click', function (event) {
      event.preventDefault();
      showBar();
    });
  });

  var choice = readChoice();
  if (choice === 'granted') {
    loadMetrika();
  } else if (choice !== 'denied') {
    showBar();
  }

  window.pfConsent = {
    open: showBar,
    load: loadMetrika,
    choice: readChoice
  };
})();
