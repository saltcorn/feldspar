# Reference values for the plot stats (analytics TODO A2.4), recorded from R.
#
#   Rscript crates/sc-analytics/tests/r/plot_reference.R
#
# writes plot_reference.json next to this script, which the tests in
# crates/sc-analytics/src/plot/math.rs read. Base R only (no jsonlite), and
# only R's own data sets, so anyone can re-record it.

json <- function(x) {
  if (is.list(x)) {
    if (is.null(names(x))) {
      return(paste0("[", paste(vapply(x, json, ""), collapse = ","), "]"))
    }
    parts <- vapply(names(x), function(k) paste0('"', k, '":', json(x[[k]])), "")
    return(paste0("{", paste(parts, collapse = ","), "}"))
  }
  if (is.character(x)) return(paste0('"', x, '"'))
  num <- function(v) if (is.na(v)) "null" else sprintf("%.17g", v)
  if (length(x) == 1 && is.null(attr(x, "array"))) return(num(x))
  paste0("[", paste(vapply(x, num, ""), collapse = ","), "]")
}
# Always an array, even of one.
arr <- function(x) structure(x, array = TRUE)

# quantile(type = 7), the default.
qx <- c(2, 4, 4, 4, 5, 5, 7, 9, 13.5, -1)
qp <- c(0, 0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 1)
quantiles <- list(x = arr(qx), p = arr(qp), q = arr(unname(quantile(qx, qp))))

# t.test's interval for a mean.
tx <- c(2, 4, 4, 4, 5, 5, 7, 9)
intervals <- lapply(c(0.95, 0.9, 0.99), function(level) {
  list(x = arr(tx), level = level, ci = arr(as.vector(t.test(tx, conf.level = level)$conf.int)))
})

# bw.nrd0 and density() on two of R's data sets, with the exact Gaussian
# estimate at the same grid (density() bins the data, so it is only close).
dens <- function(name, x) {
  d <- density(x)
  exact <- vapply(d$x, function(g) mean(dnorm(g, x, d$bw)), 0)
  list(name = name, x = arr(x), n = length(x), sd = sd(x), iqr = IQR(x), bw = d$bw,
       grid = arr(d$x), density = arr(d$y), exact = arr(exact))
}
densities <- list(dens("cars$speed", cars$speed), dens("faithful$eruptions", faithful$eruptions))

# lm and its confidence band at 80 points, as the linear smoother draws it.
lm_band <- function(name, x, y, level) {
  grid <- seq(min(x), max(x), length.out = 80)
  m <- lm(y ~ x)
  p <- predict(m, data.frame(x = grid), interval = "confidence", level = level)
  list(name = name, x = arr(x), y = arr(y), level = level,
       intercept = unname(coef(m)[1]), slope = unname(coef(m)[2]), sigma = summary(m)$sigma,
       grid = arr(grid), fit = arr(unname(p[, "fit"])),
       lower = arr(unname(p[, "lwr"])), upper = arr(unname(p[, "upr"])))
}
linear <- list(lm_band("dist ~ speed, cars", cars$speed, cars$dist, 0.95),
               lm_band("eruptions ~ waiting, faithful", faithful$waiting, faithful$eruptions, 0.9))

# loess(degree = 2, surface = "direct") with its default statistics, and the
# band ggplot2 draws from predict(se = TRUE): fit ± qt(level/2 + 0.5, df)·se.
loess_band <- function(name, x, y, span, level) {
  grid <- seq(min(x), max(x), length.out = 80)
  m <- loess(y ~ x, span = span, degree = 2, control = loess.control(surface = "direct"))
  p <- predict(m, data.frame(x = grid), se = TRUE)
  t <- qt(level / 2 + 0.5, p$df)
  list(name = name, x = arr(x), y = arr(y), span = span, level = level,
       trace = m$trace.hat, one_delta = m$one.delta, two_delta = m$two.delta,
       s = m$s, df = p$df, grid = arr(grid), fit = arr(unname(p$fit)),
       se = arr(unname(p$se.fit)),
       lower = arr(unname(p$fit - t * p$se.fit)), upper = arr(unname(p$fit + t * p$se.fit)))
}
smoothers <- list(
  loess_band("dist ~ speed, cars", cars$speed, cars$dist, 0.75, 0.95),
  loess_band("eruptions ~ waiting, faithful", faithful$waiting, faithful$eruptions, 0.3, 0.95),
  loess_band("dist ~ speed, cars, span 2", cars$speed, cars$dist, 2, 0.9)
)

out <- list(r_version = R.version.string, quantiles = quantiles, intervals = intervals,
            densities = densities, linear = linear, loess = smoothers)
args <- commandArgs(trailingOnly = FALSE)
here <- dirname(normalizePath(sub("^--file=", "", args[grep("^--file=", args)])))
writeLines(json(out), file.path(here, "plot_reference.json"))
