/* Site Restorer — цели Яндекс.Метрики на действия посетителя.
   Срабатывают только если счётчик загружен (то есть посетитель не отключил статистику).
   Цели, которые нужно завести в интерфейсе Метрики (тип «JavaScript-событие»):
     download, support, copy_cmd, consent_off */
(function () {
  'use strict';

  var COUNTER_ID = 114170389;

  function send(goal, params) {
    if (typeof window.ym !== 'function') return;
    try {
      window.ym(COUNTER_ID, 'reachGoal', goal, params);
    } catch (error) {
      /* счётчик мог не загрузиться — молча выходим */
    }
  }

  document.addEventListener('click', function (event) {
    var target = event.target;
    if (!target || !target.closest) return;
    var link = target.closest('a[href]');

    if (link) {
      var href = link.getAttribute('href') || '';
      var asset = href.match(/releases\/(?:latest\/download|download\/[^/]+)\/([^/?#]+)/);
      if (asset) {
        var name = asset[1];
        var ext = name.indexOf('.') > -1 ? name.split('.').pop().toLowerCase() : 'bin';
        send('download', { file: name, ext: ext });
        return;
      }
      if (/boosty\.to/i.test(href)) {
        send('support');
        return;
      }
    }

    var copy = target.closest('[data-copy]');
    if (copy) {
      var what = copy.getAttribute('data-copy');
      send('copy_cmd', { what: what === 'self' ? 'inline' : what });
    }
  }, true);
})();
