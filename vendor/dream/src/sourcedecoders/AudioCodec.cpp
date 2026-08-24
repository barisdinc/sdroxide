/******************************************************************************\
 *
 * Copyright (c) 2013
 *
 * Author(s):
 *  David Flamand
 *
 * Description:
 *  Audio codec base class
 *
 ******************************************************************************
 *
 * This program is free software; you can redistribute it and/or modify it under
 * the terms of the GNU General Public License as published by the Free Software
 * Foundation; either version 2 of the License, or (at your option) any later
 * version.
 *
 * This program is distributed in the hope that it will be useful, but WITHOUT
 * ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
 * FOR A PARTICULAR PURPOSE. See the GNU General Public License for more
 * details.
 *
 * You should have received a copy of the GNU General Public License along with
 * this program; if not, write to the Free Software Foundation, Inc.,
 * 59 Temple Place, Suite 330, Boston, MA 02111-1307 USA
 *
\******************************************************************************/

#include "AudioCodec.h"
#include <mutex>
#include "null_codec.h"
#include "aac_codec.h"
#include "opus_codec.h"
#ifdef HAVE_LIBFDK_AAC
# include "fdk_aac_codec.h"
#endif

CAudioCodec::CAudioCodec():pFile(nullptr)
{

}

CAudioCodec::~CAudioCodec() {

}

/* One codec list per thread, not one per process.
 *
 * Upstream shares both the list and the codec objects in it between every
 * CAudioSourceDecoder in the program, with a plain `int` reference count and no
 * lock at all. That is safe for a single-receiver console application and for
 * a Qt GUI, where everything happens on one thread; it is not safe for a host
 * that runs a receiver per thread, which is what sdroxide does — one for each
 * radio, plus an overlapping pair for the moment a receiver is being replaced.
 * Two threads in InitCodecList at once double-free the vector's storage, and
 * that is only the first thing to go wrong: GetDecoder hands both receivers the
 * *same* AacCodec, so one receiver's DecClose frees the faad2 handle the other
 * is decoding through.
 *
 * Per-thread lists fix both at once, and the invariant already holds — a Dream
 * receiver may only be used from the thread that built it, so its decoder and
 * its codec live and die on that thread together. */
thread_local vector<CAudioCodec*>
CAudioCodec::CodecList;

thread_local int
CAudioCodec::RefCount = 0;

/* Constructing a codec can still dlopen a shared library and fill a table of
   *global* function pointers with the symbols out of it (see opus_codec.cpp),
   so construction and destruction are serialised even though the list is not
   shared. Nothing on the decode path takes this. */
static std::mutex CodecListMutex;

void
CAudioCodec::InitCodecList()
{
	std::lock_guard<std::mutex> lock(CodecListMutex);
	if (CodecList.size() == 0)
	{
		/* Null codec, MUST be the first */
		CodecList.push_back(new NullCodec);

		/* AAC */
#ifdef HAVE_LIBFDK_AAC
        CodecList.push_back(new FdkAacCodec);
#endif
        CodecList.push_back(new AacCodec);

		/* Opus */
		CodecList.push_back(new OpusCodec);
	}
	RefCount ++;
}

void
CAudioCodec::UnrefCodecList()
{
	std::lock_guard<std::mutex> lock(CodecListMutex);
	RefCount --;
	if (!RefCount)
	{
		while (CodecList.size() != 0)
		{
			delete CodecList.back();
			CodecList.pop_back();
		}
	}
}

CAudioCodec*
CAudioCodec::GetDecoder(CAudioParam::EAudCod eAudioCoding, bool bCanReturnNullPtr)
{
    const int size = int(CodecList.size());
	for (int i = 1; i < size; i++)
        if (CodecList[unsigned(i)]->CanDecode(eAudioCoding))
            return CodecList[unsigned(i)];
	/* Fallback to null codec */
    return bCanReturnNullPtr ? nullptr : CodecList[0]; // ie the null codec
}

CAudioCodec*
CAudioCodec::GetEncoder(CAudioParam::EAudCod eAudioCoding, bool bCanReturnNullPtr)
{
	const int size = CodecList.size();
	for (int i = 1; i < size; i++)
		if (CodecList[i]->CanEncode(eAudioCoding))
			return CodecList[i];
	/* Fallback to null codec */
    return bCanReturnNullPtr ? nullptr : CodecList[0]; // ie the null codec
}

void
CAudioCodec::Init(const CAudioParam&, int)
{
}

void
CAudioCodec::openFile(const CParameter& Parameters)
{
    if(pFile != nullptr) {
        fclose(pFile);
        pFile = nullptr;
    }
    string fn = fileName(Parameters);
    pFile = fopen(fn.c_str(), "wb");
}

void
CAudioCodec::writeFile(const vector<uint8_t>& audio_frame)
{
    if (pFile!=nullptr)
    {
        size_t iNewFrL = size_t(audio_frame.size()) + 1;
        fwrite(&iNewFrL, size_t(4), size_t(1), pFile);	// frame length
        fwrite(&audio_frame[0], 1, iNewFrL, pFile);	// data
        fflush(pFile);
    }
}

void
CAudioCodec::closeFile() {
    if(pFile != nullptr) {
        fclose(pFile);
        pFile = nullptr;
    }
}
