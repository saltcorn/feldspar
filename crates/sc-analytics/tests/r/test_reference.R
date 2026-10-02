# Reference values for the hypothesis tests (analytics TODO A2.12), recorded
# from R.
#
#   Rscript crates/sc-analytics/tests/r/test_reference.R
#
# writes test_reference.json next to this script, which the tests in
# crates/sc-analytics/src/stats/ read. Base R and MASS only (no jsonlite, no
# car), and only data sets that ship with R, so anyone can re-record it.
#
# Each case is the test's inputs and what R answers for them; where R has no
# function (Levene's test, the effect sizes) the case computes it from base R
# as the code documents it.

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
  num <- function(v) if (is.na(v)) "null" else if (is.infinite(v)) (if (v > 0) "1e308" else "-1e308") else sprintf("%.17g", v)
  if (length(x) == 1 && is.null(attr(x, "array"))) return(num(x))
  paste0("[", paste(vapply(x, num, ""), collapse = ","), "]")
}
# Always an array, even of one.
arr <- function(x) structure(as.vector(x), array = TRUE)
ci <- function(t) arr(unname(as.vector(t$conf.int)))
p <- function(t) unname(t$p.value)
stat <- function(t) unname(t$statistic)

# --- distributions --------------------------------------------------------------

tukey <- lapply(list(c(3.5, 3, 12), c(2, 2, 5), c(4.2, 6, 60), c(1, 4, 20),
                     c(5.5, 10, 100), c(3.31, 3, 2000), c(0.4, 2, 30)), function(a) {
  list(q = a[1], k = a[2], df = a[3], p = ptukey(a[1], a[2], a[3]),
       upper = ptukey(a[1], a[2], a[3], lower.tail = FALSE))
})
qtukeys <- lapply(list(c(0.95, 3, 12), c(0.95, 6, 60), c(0.99, 4, 20), c(0.9, 2, 5)),
                  function(a) list(p = a[1], k = a[2], df = a[3], q = qtukey(a[1], a[2], a[3])))

# --- one continuous column ---------------------------------------------------------

one_t <- function(name, x, mu) {
  t <- t.test(x, mu = mu)
  list(name = name, x = arr(x), mu = mu, t = stat(t), df = unname(t$parameter), p = p(t),
       mean = unname(t$estimate), ci = ci(t), d = (mean(x) - mu) / sd(x))
}
one_sample_t <- list(one_t("sleep$extra", sleep$extra, 0), one_t("mtcars$mpg, mu 20", mtcars$mpg, 20))

sw <- function(name, x) {
  t <- shapiro.test(x)
  list(name = name, x = arr(x), w = stat(t), p = p(t))
}
shapiro <- list(sw("faithful$eruptions", faithful$eruptions), sw("mtcars$mpg", mtcars$mpg),
                sw("precip", unname(precip)), sw("three", c(1, 2, 4)),
                sw("seven", c(2.1, 3.4, 1.9, 5.6, 4.4, 3.0, 2.2)),
                sw("rivers", rivers))

signed <- function(name, x, mu) {
  t <- suppressWarnings(wilcox.test(x, mu = mu, conf.int = TRUE))
  list(name = name, x = arr(x), mu = mu, v = stat(t), p = p(t),
       estimate = unname(t$estimate), ci = ci(t), method = t$method)
}
x_paired <- c(1.83, 0.50, 1.62, 2.48, 1.68, 1.88, 1.55, 3.06, 1.30)
y_paired <- c(0.878, 0.647, 0.598, 2.05, 1.06, 1.29, 1.06, 3.14, 1.29)
signed_rank <- list(
  signed("depression, exact", x_paired - y_paired, 0),
  signed("sleep differences, ties and a zero", sleep$extra[11:20] - sleep$extra[1:10], 0),
  signed("anorexia gain, 72", MASS::anorexia$Postwt - MASS::anorexia$Prewt, 0),
  signed("mtcars$mpg, mu 20", mtcars$mpg, 20)
)

# --- one categorical column --------------------------------------------------------

gof <- function(name, counts) {
  t <- chisq.test(counts)
  list(name = name, counts = arr(counts), x2 = stat(t), df = unname(t$parameter), p = p(t),
       w = sqrt(stat(t) / sum(counts)))
}
chisq_fit <- list(gof("mtcars$cyl", as.vector(table(mtcars$cyl))),
                  gof("esoph$agegp", as.vector(table(esoph$agegp))),
                  gof("uniform", c(18, 18, 18)))

bt <- function(x, n) {
  t <- binom.test(x, n)
  list(x = x, n = n, p = p(t), estimate = unname(t$estimate), ci = ci(t),
       h = 2 * asin(sqrt(x / n)) - 2 * asin(sqrt(0.5)))
}
binomials <- list(bt(7, 20), bt(682, 925), bt(10, 10), bt(0, 4), bt(500300, 1000000), bt(13, 26))

# --- a number by two groups ----------------------------------------------------------

welch <- function(name, x, y) {
  t <- t.test(x, y)
  sp <- sqrt(((length(x) - 1) * var(x) + (length(y) - 1) * var(y)) / (length(x) + length(y) - 2))
  list(name = name, x = arr(x), y = arr(y), t = stat(t), df = unname(t$parameter), p = p(t),
       difference = mean(x) - mean(y), ci = ci(t), d = (mean(x) - mean(y)) / sp)
}
welch_t <- list(welch("sleep by group", sleep$extra[1:10], sleep$extra[11:20]),
                welch("mtcars$mpg by am", mtcars$mpg[mtcars$am == 0], mtcars$mpg[mtcars$am == 1]))

mw <- function(name, x, y) {
  t <- suppressWarnings(wilcox.test(x, y, conf.int = TRUE))
  w <- stat(t)
  list(name = name, x = arr(x), y = arr(y), w = w, p = p(t), estimate = unname(t$estimate),
       ci = ci(t), method = t$method, r = 2 * w / (length(x) * length(y)) - 1)
}
mann_whitney <- list(
  mw("permeability, exact", c(0.80, 0.83, 1.89, 1.04, 1.45, 1.38, 1.91, 1.64, 0.73, 1.46),
     c(1.15, 0.88, 0.90, 0.74, 1.21)),
  mw("mtcars$mpg by am, ties", mtcars$mpg[mtcars$am == 0], mtcars$mpg[mtcars$am == 1]),
  mw("faithful eruptions by waiting, large", faithful$eruptions[faithful$waiting > 70],
     faithful$eruptions[faithful$waiting <= 70])
)

# --- a number by several groups ---------------------------------------------------------

groups_case <- function(name, y, g) {
  g <- factor(g)
  fit <- aov(y ~ g)
  a <- summary(fit)[[1]]
  hsd <- TukeyHSD(fit)$g
  kw <- kruskal.test(y, g)
  dev <- abs(y - ave(y, g, FUN = median))
  lev <- summary(aov(dev ~ g))[[1]]
  ss <- a[["Sum Sq"]]
  pairs <- strsplit(rownames(hsd), "-", fixed = TRUE)
  list(name = name, levels = levels(g), y = lapply(levels(g), function(l) arr(y[g == l])),
       f = a[["F value"]][1], df = arr(a[["Df"]]), p = a[["Pr(>F)"]][1], eta2 = ss[1] / sum(ss),
       h = stat(kw), kw_df = unname(kw$parameter), kw_p = p(kw),
       epsilon2 = stat(kw) / (length(y) - 1),
       levene_f = lev[["F value"]][1], levene_p = lev[["Pr(>F)"]][1],
       tukey = lapply(seq_len(nrow(hsd)), function(i) list(
         a = pairs[[i]][2], b = pairs[[i]][1], diff = hsd[i, "diff"], lower = hsd[i, "lwr"],
         upper = hsd[i, "upr"], p = hsd[i, "p adj"])))
}
groups <- list(groups_case("warpbreaks breaks ~ tension", warpbreaks$breaks, warpbreaks$tension),
               groups_case("PlantGrowth weight ~ group", PlantGrowth$weight, PlantGrowth$group),
               groups_case("chickwts weight ~ feed", chickwts$weight, chickwts$feed))

# --- two categorical columns ---------------------------------------------------------------

# fisher.test's own conditional MLE and interval for a 2 x 2 table, solved as
# fisher.test solves them but with uniroot(tol = 1e-14): fisher.test uses the
# default tolerance (about 1e-4 on a scale where the odds ratio is 1/t), which
# leaves its upper ends off by up to a few percent.
fisher_tight <- function(tab, conf.level = 0.95) {
  m <- sum(tab[, 1L]); n <- sum(tab[, 2L]); k <- sum(tab[1L, ]); x <- tab[1L, 1L]
  lo <- max(0L, k - n); hi <- min(k, m); support <- lo:hi
  logdc <- dhyper(support, m, n, k, log = TRUE)
  dnhyper <- function(ncp) { d <- logdc + log(ncp) * support; d <- exp(d - max(d)); d / sum(d) }
  mnhyper <- function(ncp) if (ncp == 0) lo else if (ncp == Inf) hi else sum(support * dnhyper(ncp))
  pnhyper <- function(q, ncp, upper.tail = FALSE) {
    if (ncp == 1) return(if (upper.tail) phyper(x - 1, m, n, k, lower.tail = FALSE) else phyper(x, m, n, k))
    if (ncp == 0) return(as.numeric(if (upper.tail) q <= lo else q >= lo))
    if (ncp == Inf) return(as.numeric(if (upper.tail) q <= hi else q >= hi))
    sum(dnhyper(ncp)[if (upper.tail) support >= q else support <= q])
  }
  tol <- 1e-14
  eps <- .Machine$double.eps
  mle <- function(x) {
    if (x == lo) return(0)
    if (x == hi) return(Inf)
    mu <- mnhyper(1)
    if (mu > x) uniroot(function(t) mnhyper(t) - x, c(0, 1), tol = tol)$root
    else if (mu < x) 1 / uniroot(function(t) mnhyper(1 / t) - x, c(eps, 1), tol = tol)$root
    else 1
  }
  alpha <- (1 - conf.level) / 2
  ncp.U <- function(x, alpha) {
    if (x == hi) return(Inf)
    p <- pnhyper(x, 1)
    if (p < alpha) uniroot(function(t) pnhyper(x, t) - alpha, c(0, 1), tol = tol)$root
    else if (p > alpha) 1 / uniroot(function(t) pnhyper(x, 1 / t) - alpha, c(eps, 1), tol = tol)$root
    else 1
  }
  ncp.L <- function(x, alpha) {
    if (x == lo) return(0)
    p <- pnhyper(x, 1, upper.tail = TRUE)
    if (p > alpha) uniroot(function(t) pnhyper(x, t, upper.tail = TRUE) - alpha, c(0, 1), tol = tol)$root
    else if (p < alpha) 1 / uniroot(function(t) pnhyper(x, 1 / t, upper.tail = TRUE) - alpha, c(eps, 1), tol = tol)$root
    else 1
  }
  list(estimate = mle(x), ci = c(ncp.L(x, alpha), ncp.U(x, alpha)))
}
indep <- function(name, m) {
  t <- suppressWarnings(chisq.test(m, correct = FALSE))
  k <- min(dim(m))
  # FEXACT runs out of room on large tables, as the enumeration here does.
  f <- if (sum(m) <= 200) fisher.test(m, workspace = 2e7) else list(p.value = NA)
  out <- list(name = name, rows = nrow(m), table = arr(t(m)), x2 = stat(t),
              df = unname(t$parameter), p = p(t), v = sqrt(stat(t) / (sum(m) * (k - 1))),
              expected_min = min(t$expected), fisher_p = p(f))
  if (all(dim(m) == 2)) {
    out$odds_ratio <- unname(f$estimate)
    out$odds_ci <- ci(f)
    precise <- fisher_tight(m)
    out$odds_ratio_tight <- precise$estimate
    out$odds_ci_tight <- arr(precise$ci)
  }
  out
}
job <- matrix(c(1, 2, 1, 0, 3, 3, 6, 1, 10, 10, 14, 9, 6, 7, 12, 11), 4, 4)
contingency <- list(
  indep("tea tasting", matrix(c(3, 1, 1, 3), 2)),
  indep("mtcars am by vs", unclass(table(mtcars$am, mtcars$vs))),
  indep("mtcars cyl by am", unclass(table(mtcars$cyl, mtcars$am))),
  indep("job satisfaction", job),
  indep("a zero", matrix(c(0, 5, 6, 2), 2)),
  indep("hair by eye", unclass(apply(HairEyeColor, c(1, 2), sum)))
)

# --- two numbers --------------------------------------------------------------------------------

corr <- function(name, x, y) {
  pe <- cor.test(x, y)
  sp <- suppressWarnings(cor.test(x, y, method = "spearman"))
  m <- summary(lm(y ~ x))
  co <- m$coefficients
  cf <- confint(lm(y ~ x))
  list(name = name, x = arr(x), y = arr(y),
       r = unname(pe$estimate), r_t = stat(pe), r_df = unname(pe$parameter), r_p = p(pe),
       r_ci = ci(pe), rho = unname(sp$estimate), rho_s = stat(sp), rho_p = p(sp),
       intercept = co[1, 1], slope = co[2, 1], slope_se = co[2, 2], slope_t = co[2, 3],
       slope_p = co[2, 4], slope_ci = arr(cf[2, ]), r2 = m$r.squared)
}
sx <- sin(1:30)
sy <- cos((1:30) * 0.7) + (1:30) / 30
correlation <- list(
  corr("cars dist ~ speed, ties", cars$speed, cars$dist),
  corr("cor.test example, nine, exact", c(44.4, 45.9, 41.9, 53.3, 44.7, 44.1, 50.7, 45.2, 60.1),
       c(2.6, 3.1, 2.5, 5.0, 3.6, 4.0, 5.2, 2.8, 3.8)),
  corr("thirty, no ties, Edgeworth", sx, sy),
  corr("faithful waiting, eruptions", faithful$waiting, faithful$eruptions)
)

logit <- function(name, x, y) {
  fit <- glm(y ~ x, family = binomial)
  co <- summary(fit)$coefficients
  wald <- confint.default(fit)
  list(name = name, x = arr(x), y = arr(as.numeric(y)),
       intercept = co[1, 1], slope = co[2, 1], slope_se = co[2, 2], slope_z = co[2, 3],
       slope_p = co[2, 4], lr = fit$null.deviance - fit$deviance,
       lr_p = pchisq(fit$null.deviance - fit$deviance, 1, lower.tail = FALSE),
       odds_ratio = exp(co[2, 1]), odds_ci = arr(exp(wald[2, ])),
       mcfadden = 1 - fit$deviance / fit$null.deviance)
}
logistic <- list(logit("mtcars am ~ wt", mtcars$wt, mtcars$am),
                 logit("mtcars vs ~ hp", mtcars$hp, mtcars$vs),
                 logit("faithful long ~ waiting", faithful$waiting, faithful$eruptions > 3))

# --- paired -------------------------------------------------------------------------------------

pair <- function(name, x, y) {
  t <- t.test(x, y, paired = TRUE)
  d <- x - y
  list(name = name, x = arr(x), y = arr(y), t = stat(t), df = unname(t$parameter), p = p(t),
       mean = unname(t$estimate), ci = ci(t), d = mean(d) / sd(d))
}
paired_t <- list(pair("depression", x_paired, y_paired),
                 pair("sleep", sleep$extra[11:20], sleep$extra[1:10]),
                 pair("anorexia", MASS::anorexia$Postwt, MASS::anorexia$Prewt))

out <- list(r_version = R.version.string, tukey = tukey, qtukey = qtukeys,
            one_sample_t = one_sample_t, shapiro = shapiro, signed_rank = signed_rank,
            chisq_fit = chisq_fit, binomial = binomials, welch_t = welch_t,
            mann_whitney = mann_whitney, groups = groups, contingency = contingency,
            correlation = correlation, logistic = logistic, paired_t = paired_t)
args <- commandArgs(trailingOnly = FALSE)
here <- dirname(normalizePath(sub("^--file=", "", args[grep("^--file=", args)])))
writeLines(json(out), file.path(here, "test_reference.json"))
