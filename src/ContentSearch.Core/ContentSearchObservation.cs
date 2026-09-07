using System;
using System.Threading;

namespace PersonalRag.ContentSearch.Core;

/// <summary>
/// Measurement-only hook invoked at the exact-content verification boundary.
/// Production behavior is unchanged when no observer is installed.
/// </summary>
public static class ContentSearchObservation
{
    private static readonly AsyncLocal<Action<ContentMatch>?> Current = new();

    public static IDisposable Push(Action<ContentMatch> observer)
    {
        ArgumentNullException.ThrowIfNull(observer);
        var previous = Current.Value;
        Current.Value = observer;
        return new Scope(() => Current.Value = previous);
    }

    internal static void Report(ContentMatch match)
        => Current.Value?.Invoke(match);

    private sealed class Scope(Action dispose) : IDisposable
    {
        private Action? _dispose = dispose;

        public void Dispose()
            => Interlocked.Exchange(ref _dispose, null)?.Invoke();
    }
}
