/* Site Restorer — landing page scripts.
   Без зависимостей: мобильное меню, копирование команд, год в подвале. */
(function () {
  'use strict';

  // Мобильное меню
  var menuBtn = document.querySelector('[data-menu-toggle]');
  var nav = document.getElementById('site-nav');
  if (menuBtn && nav) {
    menuBtn.addEventListener('click', function () {
      var open = nav.classList.toggle('is-open');
      menuBtn.setAttribute('aria-expanded', open ? 'true' : 'false');
    });
    nav.addEventListener('click', function (event) {
      if (event.target.closest('a')) {
        nav.classList.remove('is-open');
        menuBtn.setAttribute('aria-expanded', 'false');
      }
    });
  }

  // Копирование команд
  document.querySelectorAll('[data-copy]').forEach(function (btn) {
    btn.addEventListener('click', function () {
      var from = btn.getAttribute('data-copy');
      var source = from === 'self' ? btn.dataset.value : document.getElementById(from);
      if (!source) return;
      var text = source.dataset.value || source.textContent;
      text = text.trim();

      var done = function (ok) {
        var label = btn.querySelector('[data-copy-label]');
        var original = label ? label.textContent : '';
        btn.dataset.copied = ok ? 'true' : 'false';
        if (label) label.textContent = ok ? (btn.dataset.copiedText || 'Скопировано') : (btn.dataset.failText || 'Не вышло');
        setTimeout(function () {
          btn.dataset.copied = 'false';
          if (label) label.textContent = original;
        }, 1800);
      };

      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(function () { done(true); }, function () { done(false); });
      } else {
        done(false);
      }
    });
  });

  // Год в подвале
  document.querySelectorAll('[data-year]').forEach(function (el) {
    el.textContent = String(new Date().getFullYear());
  });
})();
