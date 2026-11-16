// Демонстраційні дані каталогу. Формат записів повністю збігається з публічним API
// (docs/design.md, розділ 12.1). Ціни та наявність — вигадані, для перевірки верстки.

import rainwayPhoto from '@/assets/img/rainway.webp'

const cid = (n) => `c0000000-0000-4000-8000-${String(n).padStart(12, '0')}`
const pid = (n) => `a0000000-0000-4000-8000-${String(n).padStart(12, '0')}`

const UNITS = {
  pcs: { code: 'pcs', short_name: 'шт', allow_fraction: false },
  bag: { code: 'bag', short_name: 'мішок', allow_fraction: false },
  sheet: { code: 'sheet', short_name: 'лист', allow_fraction: false },
  kg: { code: 'kg', short_name: 'кг', allow_fraction: true },
  m2: { code: 'm2', short_name: 'м²', allow_fraction: true },
  m3: { code: 'm3', short_name: 'м³', allow_fraction: true },
}

export const categories = [
  { id: cid(1), parent_id: null, name: 'Пиломатеріали', slug: 'pylomaterialy', sort_order: 1 },
  { id: cid(11), parent_id: cid(1), name: 'Дошка', slug: 'doshka', sort_order: 1 },
  { id: cid(12), parent_id: cid(1), name: 'Брус', slug: 'brus', sort_order: 2 },

  { id: cid(2), parent_id: null, name: 'Цемент і сухі суміші', slug: 'tsement-i-sumishi', sort_order: 2 },
  { id: cid(21), parent_id: cid(2), name: 'Цемент', slug: 'tsement', sort_order: 1 },
  { id: cid(22), parent_id: cid(2), name: 'Штукатурки та клеї', slug: 'shtukaturky-ta-kleyi', sort_order: 2 },

  { id: cid(3), parent_id: null, name: 'Стінові матеріали', slug: 'stinovi-materialy', sort_order: 3 },
  { id: cid(31), parent_id: cid(3), name: 'Цегла', slug: 'tsehla', sort_order: 1 },
  { id: cid(32), parent_id: cid(3), name: 'Газобетон', slug: 'hazobeton', sort_order: 2 },

  { id: cid(4), parent_id: null, name: 'Покрівля', slug: 'pokrivlia', sort_order: 4 },
  { id: cid(41), parent_id: cid(4), name: 'Профнастил', slug: 'profnastyl', sort_order: 1 },
  { id: cid(42), parent_id: cid(4), name: 'Водостічні системи', slug: 'vodostichni-systemy', sort_order: 2 },

  { id: cid(5), parent_id: null, name: 'Кріплення', slug: 'kriplennia', sort_order: 5 },
  { id: cid(51), parent_id: cid(5), name: 'Шурупи', slug: 'shurupy', sort_order: 1 },
  { id: cid(52), parent_id: cid(5), name: 'Цвяхи', slug: 'tsviakhy', sort_order: 2 },
  { id: cid(53), parent_id: cid(5), name: 'Болти', slug: 'bolty', sort_order: 3 },

  { id: cid(6), parent_id: null, name: 'Гіпсокартон і профілі', slug: 'hipsokarton-i-profili', sort_order: 6 },
  { id: cid(7), parent_id: null, name: 'Лакофарбові матеріали', slug: 'lakofarbovi-materialy', sort_order: 7 },
]

const rainwayImages = [{ url: rainwayPhoto, thumb_url: rainwayPhoto }]

function product(n, categoryN, name, unit, price, availability, description, extra = {}) {
  return {
    id: pid(n),
    name,
    description,
    category_id: cid(categoryN),
    unit: UNITS[unit],
    price,
    pack: null,
    images: [],
    availability,
    ...extra,
  }
}

export const products = [
  product(1, 11, 'Дошка обрізна 25×100×4000 мм', 'pcs', '96.00', 'in_stock',
    'Соснова дошка природної вологості. Підходить для обрешітки, опалубки та чорнових підлог.'),
  product(2, 11, 'Дошка обрізна 50×150×6000 мм', 'pcs', '430.00', 'in_stock',
    'Соснова дошка для крокв, балок перекриття та каркасних конструкцій.'),
  product(3, 11, 'Дошка обрізна, сосна', 'm3', '9800.00', 'coming_soon',
    'Продаж на об’єм. Перерізи та довжину уточнює менеджер під ваше замовлення.'),
  product(4, 12, 'Брус 100×100×4000 мм', 'pcs', '620.00', 'in_stock',
    'Сосновий брус для каркасів, мауерлатів і стійок.'),

  product(5, 21, 'Цемент ПЦ-400, 25 кг', 'bag', '185.00', 'in_stock',
    'Портландцемент загальнобудівельного призначення: стяжки, кладка, штукатурні розчини.'),
  product(6, 21, 'Цемент ПЦ-500, 25 кг', 'bag', '210.00', 'in_stock',
    'Цемент підвищеної міцності для фундаментів і відповідальних бетонних конструкцій.'),
  product(7, 22, 'Штукатурка гіпсова, 30 кг', 'bag', '320.00', 'in_stock',
    'Гіпсова штукатурка для вирівнювання стін і стель усередині приміщень.'),
  product(8, 22, 'Клей для плитки, 25 кг', 'bag', '245.00', 'out_of_stock',
    'Клей для керамічної плитки на стіни та підлогу всередині та зовні будівель.'),

  product(9, 31, 'Цегла рядова повнотіла М-100', 'pcs', '9.50', 'in_stock',
    'Керамічна цегла для несучих стін, перегородок і цоколів.'),
  product(10, 31, 'Цегла облицювальна червона', 'pcs', '18.00', 'coming_soon',
    'Лицьова пустотіла цегла з рівними гранями для фасадів і парканів.'),
  product(11, 32, 'Газобетон 600×200×300 мм', 'pcs', '115.00', 'in_stock',
    'Стіновий газобетонний блок густиною D500. Легкий, теплий, точної геометрії.'),

  product(12, 41, 'Профнастил ПС-20, 0,45 мм, графіт', 'm2', '265.00', 'in_stock',
    'Профільований лист із полімерним покриттям, колір RAL 7024. Для покрівлі та парканів.'),
  product(13, 41, 'Профнастил ПС-8, 0,40 мм, оцинкований', 'm2', '198.00', 'in_stock',
    'Оцинкований профільований лист для огорож і господарських споруд.'),
  product(14, 42, 'Ринва Rainway 90 мм, 3 м, коричнева', 'pcs', '310.00', 'in_stock',
    'Ринва водостічної системи Rainway. Елементи з’єднуються без додаткових інструментів.',
    { images: rainwayImages }),
  product(15, 42, 'Труба водостічна Rainway 75 мм, 3 м, коричнева', 'pcs', '345.00', 'in_stock',
    'Водостічна труба системи Rainway з ПВХ, стійка до ультрафіолету та морозу.',
    { images: rainwayImages }),
  product(16, 42, 'Лійка ринви Rainway 90/75, коричнева', 'pcs', '165.00', 'coming_soon',
    'Лійка для з’єднання ринви 90 мм із трубою 75 мм.',
    { images: rainwayImages }),

  product(17, 51, 'Шуруп по дереву 3,5×35 мм', 'pcs', '0.45', 'in_stock',
    'Чорний фосфатований шуруп з рідким кроком різьби. Продається поштучно або пачкою.',
    { pack: { size: 1000, price: '380.00' } }),
  product(18, 51, 'Шуруп по металу 4,2×16 мм зі свердлом', 'pcs', '0.80', 'in_stock',
    'Оцинкований шуруп із пресшайбою для кріплення листового металу.',
    { pack: { size: 500, price: '350.00' } }),
  product(19, 52, 'Цвяхи будівельні 3,0×80 мм', 'kg', '68.00', 'in_stock',
    'Будівельні цвяхи з плоскою головкою. Продаються на вагу.'),
  product(20, 53, 'Болт М10×80 з гайкою', 'pcs', '9.00', 'out_of_stock',
    'Оцинкований болт із шестигранною головкою, клас міцності 5.8, у комплекті з гайкою.',
    { pack: { size: 50, price: '400.00' } }),

  product(21, 6, 'Гіпсокартон стіновий 12,5 мм, 1200×2500 мм', 'sheet', '265.00', 'in_stock',
    'Стандартний гіпсокартонний лист для стін і перегородок у сухих приміщеннях.'),
  product(22, 6, 'Гіпсокартон вологостійкий 12,5 мм, 1200×2500 мм', 'sheet', '330.00', 'in_stock',
    'Вологостійкий лист для ванних кімнат, кухонь і санвузлів.'),
  product(23, 6, 'Профіль CD-60, 3 м', 'pcs', '95.00', 'in_stock',
    'Стельовий профіль для каркаса під гіпсокартон.'),
  product(24, 6, 'Профіль UD-27, 3 м', 'pcs', '62.00', 'in_stock',
    'Напрямний профіль для каркаса під гіпсокартон.'),

  product(25, 7, 'Фарба інтер’єрна матова біла, 10 л', 'pcs', '890.00', 'in_stock',
    'Водно-дисперсійна фарба для стін і стель. Можна тонувати.'),
  product(26, 7, 'Емаль алкідна ПФ-115 червона, 2,8 кг', 'pcs', '410.00', 'in_stock',
    'Глянцева емаль для дерева та металу зовні та всередині приміщень.'),
  product(27, 7, 'Ґрунтовка глибокого проникнення, 10 л', 'pcs', '320.00', 'coming_soon',
    'Зміцнює основу та зменшує витрату фарби, клею і штукатурки.'),
]
