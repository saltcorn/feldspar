# Reference values for milestone A2's definition of done (analytics TODO A2.16),
# recorded from R over the rows `feldspar demo analytics` makes.
#
# The demo's rows are deterministic, so R reads them once from CSV files:
#
#   createdb demo_reference
#   feldspar demo analytics --database-url "postgres:///demo_reference?host=/var/run/postgresql"
#   for t in houses measurements; do
#     psql -d demo_reference -c "\copy (select * from $t order by id) to '/tmp/$t.csv' csv header"
#   done
#   Rscript crates/sc-server/tests/r/demo_reference.R /tmp
#
# writes demo_reference.json next to this script, which
# crates/sc-server/tests/analytics_done.rs reads. Base R only.

args <- commandArgs(trailingOnly = TRUE)
dir <- if (length(args) > 0) args[1] else "/tmp"
out <- file.path(dirname(sub("^--file=", "", grep("^--file=", commandArgs(), value = TRUE))),
                 "demo_reference.json")

json <- function(x) {
  if (is.list(x)) {
    if (is.null(names(x))) {
      return(paste0("[", paste(vapply(x, json, ""), collapse = ","), "]"))
    }
    parts <- vapply(names(x), function(k) paste0('"', k, '":', json(x[[k]])), "")
    return(paste0("{", paste(parts, collapse = ","), "}"))
  }
  if (is.character(x)) {
    if (length(x) == 1 && is.null(attr(x, "array"))) return(paste0('"', x, '"'))
    return(paste0("[", paste0('"', x, '"', collapse = ","), "]"))
  }
  if (is.logical(x) && length(x) == 1 && !is.na(x)) return(if (x) "true" else "false")
  num <- function(v) if (is.na(v)) "null" else sprintf("%.17g", v)
  if (length(x) == 1 && is.null(attr(x, "array"))) return(num(x))
  paste0("[", paste(vapply(x, num, ""), collapse = ","), "]")
}
arr <- function(x) structure(as.vector(unname(x)), array = TRUE)
test <- function(t) list(statistic = unname(t$statistic), p = unname(t$p.value))

houses <- read.csv(file.path(dir, "houses.csv"))
sold <- houses[!is.na(houses$price), ]
hood <- factor(sold$neighbourhood)

# Step 2: the scatter plot's points are the houses with a price.
scatter <- list(n = nrow(sold))

# Step 4: the linear smoother over every house with a price.
fit <- lm(price ~ area, data = sold)
smoother <- list(intercept = unname(coef(fit)[1]), slope = unname(coef(fit)[2]),
                 x_min = min(sold$area), x_max = max(sold$area))

# Step 5: the box plot's statistics per neighbourhood — quartiles of type 7
# (what the server's percentiles are), and whiskers to the furthest value
# within 1.5 interquartile ranges.
boxes <- lapply(split(sold$price, hood), function(p) {
  q <- unname(quantile(p, c(0.25, 0.5, 0.75), type = 7))
  iqr <- q[3] - q[1]
  inside <- p[p >= q[1] - 1.5 * iqr & p <= q[3] + 1.5 * iqr]
  list(n = length(p), q1 = q[1], median = q[2], q3 = q[3],
       lower = min(inside), upper = max(inside),
       outliers = arr(sort(p[p < q[1] - 1.5 * iqr | p > q[3] + 1.5 * iqr])))
})
names(boxes) <- NULL

a <- summary(aov(price ~ hood, data = sold))[[1]]
anova <- list(statistic = a[["F value"]][1], df = arr(a[["Df"]]), p = a[["Pr(>F)"]][1])
kruskal <- test(kruskal.test(price ~ hood, data = sold))
tk <- TukeyHSD(aov(price ~ hood, data = sold))$hood
tukey <- lapply(seq_len(nrow(tk)), function(i) {
  pair <- strsplit(rownames(tk)[i], "-")[[1]]
  list(a = pair[2], b = pair[1], diff = tk[i, "diff"], p = tk[i, "p adj"])
})

# Step 5, filtered to the first two neighbourhoods: Welch and the rank sum.
two <- sold[sold$neighbourhood <= 2, ]
p1 <- two$price[two$neighbourhood == 1]
p2 <- two$price[two$neighbourhood == 2]
welch <- t.test(p1, p2)
welch <- c(test(welch), list(df = unname(welch$parameter), estimate = mean(p1) - mean(p2)))
rank_sum <- test(suppressWarnings(wilcox.test(p1, p2)))

# Step 6: before and after, paired.
m <- read.csv(file.path(dir, "measurements.csv"))
paired_t <- t.test(m$before, m$after, paired = TRUE)
paired_t <- c(test(paired_t), list(df = unname(paired_t$parameter),
                                   estimate = unname(paired_t$estimate),
                                   ci = arr(paired_t$conf.int)))
signed_rank <- test(suppressWarnings(wilcox.test(m$before, m$after, paired = TRUE)))

# Step 7: the summary table — rows by neighbourhood, the mean price; `n`
# counts every house, the mean only those with a price.
table <- lapply(levels(factor(houses$neighbourhood)), function(h) {
  rows <- houses[houses$neighbourhood == as.integer(h), ]
  list(neighbourhood = as.integer(h), n = nrow(rows), mean = mean(rows$price, na.rm = TRUE))
})

writeLines(json(list(
  scatter = scatter, smoother = smoother, boxes = boxes,
  anova = anova, kruskal = kruskal, tukey = tukey,
  welch = welch, rank_sum = rank_sum,
  paired_t = paired_t, signed_rank = signed_rank,
  table = table, houses = nrow(houses), measurements = nrow(m)
)), out)
cat("wrote", out, "\n")
