-- Себестоимость в итогах архивных месяцев хранится без округления: живой отчёт суммирует
-- точные значения и округляет до копеек только в конце. Округление каждой группы до 4 знаков
-- давало расхождение на копейку между отчётом до и после архивации.
ALTER TABLE report_monthly_totals
    ALTER COLUMN sale_cost TYPE numeric,
    ALTER COLUMN return_cost TYPE numeric,
    ALTER COLUMN losses TYPE numeric;
