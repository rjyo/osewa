// Benchmark figures drawn on the home page. The tables in index.astro
// carry the same numbers; README.md and bench/RESULTS.md are the source.
const bar = (v, max) => Math.max(0, Math.min(100, v / max * 100)).toFixed(1) + '%';
const red = '#E0393E';
const redD = '#FF6A4D', grayD = '#6E6E69', gray = '#8F8F89';

export const echoRows = [
        { name: 'Cellular', cond: '60 ms · deep buffer', o: '138 ms', ow: bar(138, 2000), s: '1,714 ms', sw: bar(1714, 2000), sFill: gray, x: '12×' },
        { name: 'Wi-Fi', cond: '20 ms · 1% loss', o: '44 ms', ow: bar(44, 2000), s: '374 ms', sw: bar(374, 2000), sFill: gray, x: '8×' },
        { name: '1 Mbit/s', cond: '100 ms · 1% loss', o: '154 ms', ow: bar(154, 2000), s: 'no echo in 15 s →', sw: '100%', sFill: 'repeating-linear-gradient(45deg, #8F8F89 0 6px, #C9C9C2 6px 10px)', x: '>100×' }
      ];
export const tokyoConnect = [
        { name: 'osewa QUIC', v: '311 ms', w: bar(311, 2000), fill: redD, labelColor: '#F4F4F1' },
        { name: 'osewa WS', v: '511 ms', w: bar(511, 2000), fill: grayD, labelColor: '#C9C9C2' },
        { name: 'ssh', v: '1,337 ms', w: bar(1337, 2000), fill: grayD, labelColor: '#C9C9C2' },
        { name: 'mosh', v: '1,993 ms', w: bar(1993, 2000), fill: grayD, labelColor: '#C9C9C2' }
      ];
export const tokyoEcho = [
        { name: 'osewa QUIC', p50: 102, p95: 105, fill: redD, labelColor: '#F4F4F1' },
        { name: 'osewa WS', p50: 104, p95: 143, fill: grayD, labelColor: '#C9C9C2' },
        { name: 'mosh', p50: 113, p95: 119, fill: grayD, labelColor: '#C9C9C2' },
        { name: 'ssh', p50: 145, p95: 150, fill: grayD, labelColor: '#C9C9C2' }
      ].map(t => {
        const pos = (v) => (v - 90) / 70 * 100;
        return { ...t, l: pos(t.p50).toFixed(1) + '%', r: pos(t.p95).toFixed(1) + '%', w: (pos(t.p95) - pos(t.p50)).toFixed(1) + '%', v: t.p50 + ' / ' + t.p95 };
      });
export const resources = [
        { title: 'Host memory, idle', note: 'lower is better', rows: [
          { n: 'osewa', v: '14.3 MB', w: bar(14.3, 27.4), fill: gray },
          { n: 'mosh', v: '20.6 MB', w: bar(20.6, 27.4), fill: gray },
          { n: 'ssh', v: '27.4 MB', w: bar(27.4, 27.4), fill: gray } ] },
        { title: 'Client memory, idle', note: 'lower is better', rows: [
          { n: 'osewa', v: '6.9 MB', w: bar(6.9, 14.3), fill: gray },
          { n: 'mosh', v: '14.3 MB', w: bar(14.3, 14.3), fill: gray },
          { n: 'ssh', v: '8.2 MB', w: bar(8.2, 14.3), fill: gray } ] },
        { title: 'Host CPU per MB, log tail', note: '50 kB/s · ssh wins here', rows: [
          { n: 'osewa', v: '302 ms', w: bar(302, 434), fill: gray },
          { n: 'mosh', v: '434 ms', w: bar(434, 434), fill: gray },
          { n: 'ssh', v: '66 ms', w: bar(66, 434), fill: gray } ] },
        { title: 'Host CPU per MB, flood', note: 'output as fast as it comes', rows: [
          { n: 'osewa', v: '6 ms', w: bar(6, 45), fill: gray },
          { n: 'mosh', v: '45 ms', w: bar(45, 45), fill: gray },
          { n: 'ssh', v: '12 ms', w: bar(12, 45), fill: gray } ] }
      ].map(c => {
        const min = Math.min(...c.rows.map(r => parseFloat(r.v)));
        return { ...c, rows: c.rows.map(r => ({ ...r, fill: parseFloat(r.v) === min ? red : gray })) };
      });
