/* Sound in/out shims that hand Dream its samples from sdroxide's ring buffers
   instead of a sound card. Selected by `sound.h`'s USE_SDROXIDE_SOUND branch. */
#ifndef SDRX_SOUND_H
#define SDRX_SOUND_H

#include <sound/soundinterface.h>
#include <condition_variable>
#include <mutex>
#include <vector>

/* One decoder's two ring buffers plus the stop flag that unblocks a waiting
   Read(). Owned by the C shim; the sound objects Dream news up find theirs
   through `sdrx_current_ring()`, which the shim points at the right context
   before it touches the receiver. Every Dream call for one decoder happens on
   that decoder's own worker thread, so a thread-local is enough to keep
   concurrent decoders apart. */
class CSdrxRing
{
public:
    CSdrxRing(size_t inCapacity, size_t outCapacity)
        : in(inCapacity), out(outCapacity) {}

    /* A single-producer/single-consumer circular buffer of shorts. Reads block
       until the whole request is there; writes drop the oldest samples when the
       consumer has fallen behind, so a stalled reader cannot stall the decoder. */
    class Fifo
    {
    public:
        explicit Fifo(size_t capacity) : buf(capacity), rd(0), wr(0), used(0) {}

        size_t push(const short* src, size_t n)
        {
            std::unique_lock<std::mutex> lock(mutex);
            size_t dropped = 0;
            for (size_t i = 0; i < n; i++)
            {
                if (used == buf.size())
                {
                    rd = (rd + 1) % buf.size();
                    used--;
                    dropped++;
                }
                buf[wr] = src[i];
                wr = (wr + 1) % buf.size();
                used++;
            }
            cv.notify_all();
            return dropped;
        }

        /* Blocks until `n` samples are available or the fifo is stopped.
           Returns false when it could not fill the request. */
        bool pop_blocking(short* dst, size_t n)
        {
            std::unique_lock<std::mutex> lock(mutex);
            cv.wait(lock, [&] { return used >= n || stopped; });
            if (used < n)
                return false;
            copy_out(dst, n);
            return true;
        }

        size_t pop(short* dst, size_t n)
        {
            std::unique_lock<std::mutex> lock(mutex);
            if (n > used)
                n = used;
            copy_out(dst, n);
            return n;
        }

        size_t available()
        {
            std::unique_lock<std::mutex> lock(mutex);
            return used;
        }

        void stop()
        {
            std::unique_lock<std::mutex> lock(mutex);
            stopped = true;
            cv.notify_all();
        }

        void clear()
        {
            std::unique_lock<std::mutex> lock(mutex);
            rd = wr = 0;
            used = 0;
        }

    private:
        void copy_out(short* dst, size_t n)
        {
            for (size_t i = 0; i < n; i++)
            {
                dst[i] = buf[rd];
                rd = (rd + 1) % buf.size();
            }
            used -= n;
        }

        std::vector<short> buf;
        size_t rd, wr, used;
        bool stopped = false;
        std::mutex mutex;
        std::condition_variable cv;
    };

    Fifo in;
    Fifo out;
};

/* Set by the shim on the decoder's worker thread before it constructs or drives
   the receiver, so the sound objects Dream allocates internally find their ring. */
CSdrxRing* sdrx_current_ring();
void sdrx_set_current_ring(CSdrxRing*);

class CSoundInSdrx : public CSoundInInterface
{
public:
    CSoundInSdrx() : ring(sdrx_current_ring()) {}
    virtual ~CSoundInSdrx() {}

    virtual bool Init(int, int iNewBufferSize, bool) {
        iBufferSize = iNewBufferSize;
        return true;
    }

    /* Dream's convention is inverted: true means "bad read". */
    virtual bool Read(CVector<short>& psData) {
        if (ring == nullptr)
            return true;
        const size_t n = size_t(psData.Size());
        return !ring->in.pop_blocking(&psData[0], n);
    }

    virtual void Enumerate(std::vector<std::string>& choices,
                           std::vector<std::string>& desc, std::string& def) {
        choices.push_back(SDRX_DEVICE);
        desc.push_back("sdroxide");
        def = SDRX_DEVICE;
    }
    virtual std::string GetDev() { return SDRX_DEVICE; }
    virtual void SetDev(std::string) {}
    virtual void Close() {}
    virtual std::string GetVersion() { return "sdroxide"; }

    static const char* SDRX_DEVICE;

private:
    CSdrxRing* ring;
    int iBufferSize = 0;
};

class CSoundOutSdrx : public CSoundOutInterface
{
public:
    CSoundOutSdrx() : ring(sdrx_current_ring()) {}
    virtual ~CSoundOutSdrx() {}

    virtual bool Init(int, int, bool) { return true; }

    virtual bool Write(CVector<short>& psData) {
        if (ring == nullptr)
            return true;
        ring->out.push(&psData[0], size_t(psData.Size()));
        return false;
    }

    virtual void Enumerate(std::vector<std::string>& choices,
                           std::vector<std::string>& desc, std::string& def) {
        choices.push_back(CSoundInSdrx::SDRX_DEVICE);
        desc.push_back("sdroxide");
        def = CSoundInSdrx::SDRX_DEVICE;
    }
    virtual std::string GetDev() { return CSoundInSdrx::SDRX_DEVICE; }
    virtual void SetDev(std::string) {}
    virtual void Close() {}
    virtual std::string GetVersion() { return "sdroxide"; }

private:
    CSdrxRing* ring;
};

typedef CSoundInSdrx CSoundIn;
typedef CSoundOutSdrx CSoundOut;

#endif
