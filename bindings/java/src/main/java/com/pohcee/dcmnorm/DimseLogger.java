package com.pohcee.dcmnorm;

/**
 * Passed as {@code onLog} to {@code echoScu}/{@code storeScu}/{@code findScu}/{@code moveScu}.
 * {@link #log} is invoked synchronously, on the calling thread, once per notable DIMSE event -
 * association open/close, each request sent and response received, release/abort. Every {@code
 * *Scu} call in this binding blocks the calling thread until it completes, so {@code log} is
 * always called from inside that same blocking call, never from a background thread.
 *
 * <p>A {@code log} implementation that throws is tolerated (the exception is swallowed, not
 * propagated into the DIMSE call) but should be avoided - prefer catching internally.
 */
@FunctionalInterface
public interface DimseLogger {
    void log(String message);
}
